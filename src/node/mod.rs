mod backoff;
mod connection_state;
mod execute;
mod translate;

use std::{
    collections::{HashMap, HashSet},
    net::IpAddr,
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use anyhow::Result;
use futures::StreamExt;
use libp2p::{
    Multiaddr, PeerId, SwarmBuilder, noise,
    swarm::{
        ConnectionId, NetworkBehaviour, SwarmEvent,
        behaviour::{FromSwarm, NewExternalAddrCandidate},
    },
    yamux,
};

use self::{
    backoff::ReconnectBackoff,
    connection_state::{ConnectionStateMachine, DialingUpdate, OutgoingErrorUpdate},
    execute::{ExecutionContext, execute_commands},
    translate::translate_swarm_event,
};
use crate::{
    behaviour::Behaviour,
    external_addr,
    protocol::{self, DISCOVERY_TIMEOUT, IDLE_CONNECTION_TIMEOUT},
    shutdown,
    specs::{ExposedService, ServiceAddr, TunnelByNameSpec, TunnelSpec},
    state::{AppState, Event, NatStatus, PeerState, Phase, TunnelState},
    traits::MealyMachine,
    tunnel::{DirectPeerRegistry, TunnelRegistry},
    types::ServiceName,
};

// ─── Main entry ─────────────────────────────────────────────────────────────

// DCUtR's multi-attempt QUIC upgrade can exceed 15s in real NATs.
// Keep retries attempt-scoped, but give each attempt enough time to complete.
const HOLE_PUNCH_TIMEOUT: Duration = Duration::from_secs(30);

pub struct NodeConfig {
    pub identity_path: PathBuf,
    pub listen_addrs: Vec<Multiaddr>,
    pub bootstrap_addrs: Vec<Multiaddr>,
    pub exposed: Vec<ExposedService>,
    pub tunnel_specs: Vec<TunnelSpec>,
    pub tunnel_by_name_specs: Vec<TunnelByNameSpec>,
}

pub async fn run(config: NodeConfig) -> Result<()> {
    let keypair = crate::identity::load_or_generate(&config.identity_path)?;
    let local_peer_id = PeerId::from(keypair.public());
    tracing::info!(%local_peer_id, "starting punchgate node");

    let services: Arc<HashMap<ServiceName, ServiceAddr>> = Arc::new(
        config
            .exposed
            .iter()
            .map(|s| (s.name.clone(), s.local_addr.clone()))
            .collect(),
    );

    let bootstrap_peers_with_addrs: Vec<(PeerId, Multiaddr)> = config
        .bootstrap_addrs
        .iter()
        .filter_map(|addr| extract_peer_id(addr).map(|pid| (pid, addr.clone())))
        .collect();

    let bootstrap_peer_ids: HashSet<PeerId> = bootstrap_peers_with_addrs
        .iter()
        .map(|(pid, _)| *pid)
        .collect();

    let nat_mapping = match config.bootstrap_addrs.is_empty() {
        true => {
            tracing::info!("no bootstrap peers, skipping NAT probe");
            crate::nat_probe::NatMapping::Unknown
        }
        false => {
            tracing::info!("probing NAT type via STUN...");
            let mapping =
                crate::nat_probe::detect_nat_mapping(crate::nat_probe::DEFAULT_STUN_SERVERS).await;
            match mapping {
                crate::nat_probe::NatMapping::EndpointIndependent => {
                    tracing::info!(nat_type = %mapping, "hole-punching viable");
                }
                crate::nat_probe::NatMapping::AddressDependent => {
                    tracing::warn!(
                        nat_type = %mapping,
                        "hole-punching not viable, tunnel activation requires direct connectivity"
                    );
                }
                crate::nat_probe::NatMapping::Unknown => {
                    tracing::warn!(nat_type = %mapping, "could not determine NAT type");
                }
            }
            mapping
        }
    };

    let mut swarm = SwarmBuilder::with_existing_identity(keypair)
        .with_tokio()
        .with_quic()
        .with_relay_client(noise::Config::new, yamux::Config::default)?
        .with_behaviour(
            |key, relay_client| -> Result<Behaviour, Box<dyn std::error::Error + Send + Sync>> {
                Behaviour::new(key, relay_client).map_err(|e| e.to_string().into())
            },
        )?
        .with_swarm_config(|cfg| cfg.with_idle_connection_timeout(IDLE_CONNECTION_TIMEOUT))
        .build();

    let mut stream_control = swarm.behaviour().stream.new_control();
    let incoming = stream_control
        .accept(protocol::tunnel_protocol())
        .map_err(|_| anyhow::anyhow!("tunnel protocol already registered"))?;
    tokio::spawn(crate::tunnel::accept_loop(incoming, services.clone()));

    // Populate TunnelState with requested tunnel specs (replaces imperative HashMaps)
    let mut tunnel_state = TunnelState::new();
    for spec in &config.tunnel_specs {
        tunnel_state.add_peer_tunnel(
            spec.remote_peer,
            spec.service_name.clone(),
            spec.bind_addr.clone(),
        );
    }
    for spec in &config.tunnel_by_name_specs {
        tunnel_state.add_service_tunnel(spec.service_name.clone(), spec.bind_addr.clone());
    }
    tunnel_state.set_nat_mapping(nat_mapping);

    let mut ctx = ExecutionContext {
        active_bootstrap_query: None,
        kad_tunnel_queries: HashMap::new(),
        kad_service_queries: HashMap::new(),
        stream_control,
        direct_peers: DirectPeerRegistry::new(),
        tunnel_registry: TunnelRegistry::new(),
    };

    for addr in &config.listen_addrs {
        swarm.listen_on(addr.clone())?;
    }

    for addr in &config.bootstrap_addrs {
        tracing::info!(%addr, "dialing bootstrap peer");
        swarm.dial(addr.clone())?;
    }

    let mut peer_state = PeerState::new();
    for peer_id in &bootstrap_peer_ids {
        peer_state.bootstrap_peers.insert(*peer_id);
    }
    let mut app_state = AppState::new(peer_state, tunnel_state);

    if config.bootstrap_addrs.is_empty() {
        let old_phase = app_state.phase();
        let (new_state, commands) = app_state.transition(Event::NoBootstrapPeers);
        log_phase_transition(old_phase, new_state.phase(), commands.len());
        app_state = new_state;
        execute_commands(&mut swarm, &commands, &config.exposed, &mut ctx);
    }

    if !config.bootstrap_addrs.is_empty() {
        let old_phase = app_state.phase();
        let (new_state, commands) =
            app_state.transition(Event::NatStatusChanged(NatStatus::Private));
        log_phase_transition(old_phase, new_state.phase(), commands.len());
        app_state = new_state;
        execute_commands(&mut swarm, &commands, &config.exposed, &mut ctx);
    }

    let discovery_deadline = tokio::time::sleep(DISCOVERY_TIMEOUT);
    tokio::pin!(discovery_deadline);
    let mut discovery_timeout_fired = false;

    let mut backoff = ReconnectBackoff::new(&bootstrap_peers_with_addrs);
    let mut pending_reconnects: HashMap<PeerId, tokio::time::Instant> = HashMap::new();
    let mut observed_nat_ports: HashMap<IpAddr, HashSet<u16>> = HashMap::new();
    let mut confirmed_external_by_ip: HashMap<IpAddr, Multiaddr> = HashMap::new();
    let mut bootstrap_observed_external_by_ip: HashMap<IpAddr, Multiaddr> = HashMap::new();
    let mut announced_listen_addrs: HashSet<Multiaddr> = HashSet::new();
    let mut connection_state = ConnectionStateMachine::new();
    let mut port_mapping_handle: Option<
        tokio::task::JoinHandle<Option<crate::port_mapping::PortMapping>>,
    > = None;
    let mut port_mapping_spawned = false;

    let shutdown_signal = shutdown::shutdown_signal();
    tokio::pin!(shutdown_signal);

    loop {
        let next_reconnect = pending_reconnects.values().min().copied();
        let next_hole_punch_timeout = connection_state.next_hole_punch_deadline();

        tokio::select! {
            event = swarm.select_next_some() => {
                if let SwarmEvent::NewListenAddr { address, .. } = &event {
                    if announced_listen_addrs.insert(address.clone()) {
                        tracing::info!("listening on {address}");
                    } else {
                        tracing::debug!("ignoring duplicate listen address event: {address}");
                    }
                }

                if let SwarmEvent::NewListenAddr { address, .. } = &event
                    && !port_mapping_spawned
                    && !config.bootstrap_addrs.is_empty()
                    && let Some(port) = extract_quic_udp_port(address)
                {
                    port_mapping_spawned = true;
                    let handle = tokio::spawn(crate::port_mapping::acquire_port_mapping(port));
                    port_mapping_handle = Some(handle);
                    tracing::debug!(port, "spawned port mapping probe");
                }

                let mut synthetic_events = Vec::new();
                let mut bootstrap_fully_disconnected: Option<PeerId> = None;

                if let SwarmEvent::ConnectionEstablished { peer_id, connection_id, endpoint, .. } = &event {
                    let is_relayed = endpoint_is_relayed(endpoint);
                    log_connection_opened(peer_id, connection_id, endpoint, is_relayed);
                    let update = connection_state.on_connection_established(
                        *peer_id,
                        *connection_id,
                        is_relayed,
                    );
                    let started_attempt_id = if is_relayed
                        && app_state.tunnel.has_holepunch_intent(peer_id)
                    {
                        connection_state.start_hole_punch_attempt(*peer_id, HOLE_PUNCH_TIMEOUT)
                    } else {
                        None
                    };
                    let relayed_connections = connection_state.relayed_connection_count(peer_id);
                    let direct_connections = connection_state.direct_connection_count(peer_id);
                    if relayed_connections + direct_connections == 1 {
                        tracing::info!(
                            peer = %peer_id,
                            relayed = is_relayed,
                            relayed_connections,
                            direct_connections,
                            "peer connected"
                        );
                    }

                    if let Some(attempt_id) = started_attempt_id.or(update.started_attempt_id) {
                        synthetic_events.push(Event::HolePunchAttemptStarted {
                            peer: *peer_id,
                            attempt_id,
                        });
                        tracing::debug!(
                            peer = %peer_id,
                            attempt_id,
                            connection_id = ?connection_id,
                            timeout_secs = HOLE_PUNCH_TIMEOUT.as_secs(),
                            "relayed path active; waiting for DCUtR direct upgrade"
                        );
                    }

                    if update.direct_connection_established {
                        ctx.direct_peers.mark_direct(*peer_id).await;
                    }
                    if let Some(stats) = update.completed_attempt {
                        tracing::info!(
                            peer = %peer_id,
                            attempt_id = stats.attempt_id,
                            elapsed_ms = stats.elapsed_ms(),
                            dials_started = stats.dials_started,
                            dial_failures = stats.dials_failed,
                            last_dial_error = ?stats.last_dial_error,
                            direct_connections,
                            relayed_connections,
                            known_addrs = ?known_addrs_for_peer(&app_state, peer_id),
                            "hole punch succeeded; direct connection established"
                        );
                    }

                    for relayed_id in update.relayed_connections_to_close {
                        if swarm.close_connection(relayed_id) {
                            tracing::debug!(
                                peer = %peer_id,
                                connection_id = ?relayed_id,
                                "closed relayed connection after direct upgrade"
                            );
                        }
                    }
                }
                if let SwarmEvent::ConnectionClosed { peer_id, connection_id, endpoint, num_established, cause } = &event {
                    tracing::debug!(
                        peer = %peer_id,
                        connection_id = ?connection_id,
                        relayed = endpoint_is_relayed(endpoint),
                        remaining = num_established,
                        cause = ?cause,
                        "connection closed"
                    );
                    let update =
                        connection_state.on_connection_closed(*peer_id, *connection_id, *num_established);
                    let remaining_connections = connection_state
                        .relayed_connection_count(peer_id)
                        .saturating_add(connection_state.direct_connection_count(peer_id))
                        as u32;
                    synthetic_events.push(Event::ConnectionLost {
                        peer: *peer_id,
                        remaining_connections,
                    });

                    if update.clear_direct_registry {
                        ctx.direct_peers.clear_direct(peer_id).await;
                    }

                    if update.peer_fully_disconnected {
                        if bootstrap_peer_ids.contains(peer_id) {
                            bootstrap_fully_disconnected = Some(*peer_id);
                        }
                        tracing::info!(
                            peer = %peer_id,
                            "peer disconnected"
                        );
                    }
                }

                if let SwarmEvent::ConnectionEstablished { peer_id, .. } = &event
                    && bootstrap_peer_ids.contains(peer_id)
                {
                    backoff.reset(peer_id);
                    pending_reconnects.remove(peer_id);
                }

                if let Some(peer_id) = bootstrap_fully_disconnected
                    && let Some((_, delay)) = backoff.schedule_reconnect(peer_id)
                {
                    let deadline = tokio::time::Instant::now() + delay;
                    pending_reconnects.insert(peer_id, deadline);
                    tracing::debug!(
                        peer_id = %peer_id,
                        delay_secs = delay.as_secs(),
                        "scheduling bootstrap reconnect"
                    );
                }

                if let SwarmEvent::Dialing {
                    peer_id: Some(peer_id),
                    connection_id,
                } = &event
                {
                    match connection_state.on_dialing(*peer_id, *connection_id) {
                        DialingUpdate::Tracked {
                            attempt_id,
                            attempt,
                        } => {
                            tracing::debug!(
                                peer = %peer_id,
                                attempt_id,
                                connection_id = ?connection_id,
                                attempt,
                                relayed_connections = connection_state.relayed_connection_count(peer_id),
                                direct_connections = connection_state.direct_connection_count(peer_id),
                                "hole-punch dial attempt started"
                            );
                        }
                        DialingUpdate::RelayOnlyWithoutActiveAttempt => {
                            tracing::debug!(
                                peer = %peer_id,
                                connection_id = ?connection_id,
                                "ignoring hole-punch dial start without active attempt"
                            );
                        }
                        DialingUpdate::Ignored => {}
                    }
                }

                if let SwarmEvent::OutgoingConnectionError {
                    peer_id,
                    connection_id,
                    error,
                } = &event
                {
                    match connection_state.on_outgoing_connection_error(
                        peer_id.as_ref().copied(),
                        *connection_id,
                        error.to_string(),
                    ) {
                        OutgoingErrorUpdate::Tracked {
                            peer,
                            attempt_id,
                            attempt,
                            failures,
                        } => {
                            let error_flags = hole_punch_error_flags(error);
                            tracing::debug!(
                                peer = %peer,
                                attempt_id,
                                connection_id = ?connection_id,
                                attempt,
                                failures,
                                error = %error,
                                has_handshake_timeout = error_flags.has_handshake_timeout,
                                has_unsupported_bare_p2p = error_flags.has_unsupported_bare_p2p,
                                has_pending_abort = error_flags.has_pending_abort,
                                relayed_connections = connection_state.relayed_connection_count(&peer),
                                direct_connections = connection_state.direct_connection_count(&peer),
                                "hole-punch dial attempt failed"
                            );
                        }
                        OutgoingErrorUpdate::StaleTracked {
                            peer,
                            expected_attempt,
                            received_attempt,
                        } => {
                            tracing::debug!(
                                peer = %peer,
                                expected_attempt,
                                received_attempt,
                                connection_id = ?connection_id,
                                error = %error,
                                "ignoring stale hole-punch dial failure"
                            );
                        }
                        OutgoingErrorUpdate::RelayOnlyUntracked { peer } => {
                            tracing::debug!(
                                peer = %peer,
                                connection_id = ?connection_id,
                                error = %error,
                                "ignoring untracked dial failure while relay-only"
                            );
                        }
                        OutgoingErrorUpdate::Ignored => {}
                    }
                }

                if let SwarmEvent::OutgoingConnectionError { peer_id: Some(peer_id), .. } = &event
                    && bootstrap_peer_ids.contains(peer_id)
                    && !swarm.is_connected(peer_id)
                    && !pending_reconnects.contains_key(peer_id)
                    && let Some((_, delay)) = backoff.schedule_reconnect(*peer_id)
                {
                    pending_reconnects.insert(*peer_id, tokio::time::Instant::now() + delay);
                    tracing::debug!(
                        %peer_id,
                        delay_secs = delay.as_secs(),
                        "rescheduling bootstrap reconnect after failed dial"
                    );
                }

                if let SwarmEvent::ExternalAddrConfirmed { address } = &event {
                    if let Some((ip, _)) = external_addr::extract_public_ip_port(address) {
                        let previous = confirmed_external_by_ip.insert(ip, address.clone());
                        if let Some(prev_addr) = previous.as_ref()
                            && prev_addr != address
                        {
                            swarm.remove_external_address(prev_addr);
                            tracing::info!(
                                observed_ip = %ip,
                                old_addr = %prev_addr,
                                new_addr = %address,
                                "replaced stale confirmed external address"
                            );
                        }
                    }
                    tracing::debug!(%address, "external address confirmed");
                }

                if let SwarmEvent::ExternalAddrExpired { address } = &event {
                    if let Some((ip, _)) = external_addr::extract_public_ip_port(address)
                        && confirmed_external_by_ip
                            .get(&ip)
                            .is_some_and(|current| current == address)
                    {
                        confirmed_external_by_ip.remove(&ip);
                    }
                    tracing::warn!(%address, "external address expired");
                }

                if let SwarmEvent::NewExternalAddrCandidate { address } = &event {
                    if external_addr::is_valid_external_candidate(address) {
                        tracing::debug!(%address, "new external address candidate");
                    } else {
                        tracing::debug!(%address, "ignoring invalid external address candidate");
                    }
                }

                if let Some(handle) = &port_mapping_handle
                    && handle.is_finished()
                    && let Some(handle) = port_mapping_handle.take()
                {
                    match handle.await {
                        Ok(Some(mapping)) => {
                            // Infallible: formatted multiaddr from SocketAddr components is always valid
                            let mapped_addr: Multiaddr = format!(
                                "/ip4/{}/udp/{}/quic-v1",
                                mapping.external_addr.ip(),
                                mapping.external_addr.port()
                            )
                            .parse()
                            .expect("multiaddr from SocketAddr components is always valid");

                            swarm.add_external_address(mapped_addr.clone());
                            tracing::info!(
                                addr = %mapped_addr,
                                protocol = mapping.protocol_used,
                                "port mapping acquired — added external address, switching to Public NAT"
                            );

                            let old_phase = app_state.phase();
                            let (new_state, commands) =
                                app_state.transition(Event::NatStatusChanged(NatStatus::Public));
                            log_phase_transition(old_phase, new_state.phase(), commands.len());
                            app_state = new_state;
                            execute_commands(&mut swarm, &commands, &config.exposed, &mut ctx);
                        }
                        Ok(None) => {
                            tracing::info!("port mapping unavailable, relying on DCUtR direct connectivity");
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "port mapping task panicked");
                        }
                    }
                }

                // Translate swarm event → domain events, then feed through state machine.
                // `synthetic_events` are runtime-generated attempt lifecycle events emitted
                // from connection bookkeeping above.
                let translated_events = translate_swarm_event(
                    &event,
                    &bootstrap_peer_ids,
                    ctx.active_bootstrap_query,
                    &mut ctx.kad_tunnel_queries,
                    &mut ctx.kad_service_queries,
                    |peer| swarm.is_connected(peer),
                );

                let mut events = synthetic_events;
                events.extend(translated_events);

                for mut state_event in events {
                    if let Event::HolePunchFailed { remote_peer, reason } = &state_event {
                        if let Some(stats) = connection_state.active_attempt_stats(remote_peer) {
                            state_event = Event::HolePunchAttemptFailed {
                                remote_peer: *remote_peer,
                                attempt_id: stats.attempt_id,
                                reason: reason.clone(),
                            };
                        } else {
                            tracing::warn!(
                                peer = %remote_peer,
                                %reason,
                                nat = %nat_type_summary(nat_mapping, &observed_nat_ports),
                                "hole punch failed (DCUtR reported error without active attempt)"
                            );
                            continue;
                        }
                    }

                    // Bootstrap server: confirm observed addresses from Identify
                    // (first node has nobody to AutoNAT it)
                    if bootstrap_peer_ids.is_empty()
                        && let Event::PeerIdentified { observed_addr, .. } = &state_event
                    {
                        swarm.add_external_address(observed_addr.clone());
                    }

                    // For non-bootstrap nodes, treat direct observations by bootstrap peers as
                    // authoritative and refresh the external address advertised to peers.
                    if let Event::PeerIdentified {
                        peer,
                        observed_addr,
                        ..
                    } = &state_event
                        && bootstrap_peer_ids.contains(peer)
                        && !connection_state.has_relayed_connection(peer)
                        && external_addr::is_valid_external_candidate(observed_addr)
                        && let Some((ip, port)) = external_addr::extract_public_ip_port(observed_addr)
                    {
                        let previous = bootstrap_observed_external_by_ip
                            .insert(ip, observed_addr.clone());

                        if let Some(prev_addr) = previous.as_ref()
                            && prev_addr != observed_addr
                        {
                            swarm.remove_external_address(prev_addr);
                            tracing::info!(
                                observer = %peer,
                                observed_ip = %ip,
                                old_addr = %prev_addr,
                                new_addr = %observed_addr,
                                "bootstrap observation replaced stale external address"
                            );
                        }

                        if previous.as_ref() != Some(observed_addr) {
                            swarm.add_external_address(observed_addr.clone());
                            swarm.behaviour_mut().dcutr.on_swarm_event(
                                FromSwarm::NewExternalAddrCandidate(NewExternalAddrCandidate {
                                    addr: observed_addr,
                                }),
                            );
                            tracing::debug!(
                                observer = %peer,
                                observed_ip = %ip,
                                observed_port = port,
                                "bootstrap observation refreshed external address for DCUtR"
                            );
                        }
                    }

                    // NAT type detection: track external port observations per IP
                    // Skip relay-connected peers — their observed_addr reports the relay's address
                    if let Event::PeerIdentified {
                        peer,
                        listen_addrs,
                        observed_addr,
                    } = &state_event
                    {
                        let public_listen_addrs: Vec<_> = listen_addrs
                            .iter()
                            .filter(|addr| external_addr::has_public_ip(addr))
                            .cloned()
                            .collect();
                        let mut direct_public_ports_by_ip: HashMap<IpAddr, HashSet<u16>> =
                            HashMap::new();
                        for (ip, port) in listen_addrs
                            .iter()
                            .filter(|addr| external_addr::is_valid_external_candidate(addr))
                            .filter_map(external_addr::extract_public_ip_port)
                        {
                            direct_public_ports_by_ip.entry(ip).or_default().insert(port);
                        }
                        tracing::debug!(
                            peer = %peer,
                            observed_addr = %observed_addr,
                            listen_addrs = ?listen_addrs,
                            public_listen_addrs = ?public_listen_addrs,
                            relayed_observation_ignored = connection_state.has_relayed_connection(peer),
                            "identify received"
                        );
                        if direct_public_ports_by_ip
                            .values()
                            .any(|ports| ports.len() > 1)
                        {
                            tracing::warn!(
                                peer = %peer,
                                ports_by_ip = ?direct_public_ports_by_ip,
                                "peer advertises multiple direct public ports; hole-punch candidates may be stale"
                            );
                        }
                    }

                    // NAT type detection: track external port observations per IP
                    // Skip relay-connected peers — their observed_addr reports the relay's address
                    if let Event::PeerIdentified { peer, observed_addr, .. } = &state_event
                        && !connection_state.has_relayed_connection(peer)
                        && let Some((ip, port)) = external_addr::extract_public_ip_port(observed_addr)
                    {
                        let ports = observed_nat_ports.entry(ip).or_default();
                        if ports.insert(port) {
                            match ports.len() {
                                1 => tracing::debug!(
                                    observer = %peer,
                                    observed_ip = %ip,
                                    observed_port = port,
                                    "NAT mapping observed"
                                ),
                                _ if matches!(nat_mapping, crate::nat_probe::NatMapping::Unknown) => tracing::warn!(
                                    observer = %peer,
                                    observed_ip = %ip,
                                    observed_ports = ?ports,
                                    "symmetric NAT likely — multiple external ports for same IP, hole-punching may fail"
                                ),
                                _ if matches!(
                                    nat_mapping,
                                    crate::nat_probe::NatMapping::EndpointIndependent
                                ) => tracing::warn!(
                                    observer = %peer,
                                    observed_ip = %ip,
                                    observed_ports = ?ports,
                                    nat = %nat_mapping,
                                    "NAT mapping inconsistency: STUN says endpoint-independent but observations show multiple external ports"
                                ),
                                _ => tracing::debug!(
                                    observer = %peer,
                                    observed_ip = %ip,
                                    observed_ports = ?ports,
                                    nat = %nat_mapping,
                                    "multiple external ports observed; consistent with restricted NAT classification"
                                ),
                            }
                        }
                    }

                    match &state_event {
                        Event::DhtPeerLookupComplete { peer, connected } => {
                            tracing::debug!(peer = %peer, connected, "DHT peer lookup complete");
                        }
                        Event::DhtServiceResolved {
                            service_name,
                            providers,
                        } => {
                            tracing::info!(
                                %service_name,
                                provider_count = providers.len(),
                                providers = ?providers,
                                "DHT provider resolved service"
                            );
                        }
                        Event::DhtServiceFailed {
                            service_name,
                            reason,
                        } => {
                            tracing::warn!(
                                %service_name,
                                %reason,
                                "DHT provider lookup failed"
                            );
                        }
                        Event::HolePunchAttemptStarted { peer, attempt_id } => {
                            tracing::info!(
                                peer = %peer,
                                attempt_id,
                                timeout_secs = HOLE_PUNCH_TIMEOUT.as_secs(),
                                known_addrs = ?known_addrs_for_peer(&app_state, peer),
                                "hole punch started"
                            );
                        }
                        Event::TunnelPeerConnected { peer, relayed } => {
                            tracing::debug!(
                                peer = %peer,
                                relayed,
                                direct_connections = connection_state.direct_connection_count(peer),
                                relayed_connections = connection_state.relayed_connection_count(peer),
                                known_addrs = ?known_addrs_for_peer(&app_state, peer),
                                "tunnel peer connected state updated"
                            );
                        }
                        Event::TunnelDialFailed { peer, reason } => {
                            tracing::debug!(
                                peer = %peer,
                                %reason,
                                "tunnel dial failed event received"
                            );
                        }
                        Event::RelayReservationAccepted { relay_peer } => {
                            tracing::info!(%relay_peer, "relay reservation accepted");
                        }
                        Event::HolePunchAttemptFailed {
                            remote_peer,
                            attempt_id,
                            reason,
                        } => {
                            let known_addrs = known_addrs_for_peer(&app_state, remote_peer);
                            if let Some(stats) = connection_state.active_attempt_stats(remote_peer) {
                                if stats.attempt_id != *attempt_id {
                                    tracing::debug!(
                                        peer = %remote_peer,
                                        expected_attempt = stats.attempt_id,
                                        received_attempt = attempt_id,
                                        "ignoring stale hole-punch failure event"
                                    );
                                    continue;
                                }
                                tracing::warn!(
                                    peer = %remote_peer,
                                    attempt_id,
                                    %reason,
                                    nat = %nat_type_summary(nat_mapping, &observed_nat_ports),
                                    elapsed_ms = stats.elapsed_ms(),
                                    dials_started = stats.dials_started,
                                    dial_failures = stats.dials_failed,
                                    last_dial_error = ?stats.last_dial_error,
                                    direct_connections = connection_state.direct_connection_count(remote_peer),
                                    relayed_connections = connection_state.relayed_connection_count(remote_peer),
                                    known_addrs = ?known_addrs,
                                    "hole punch failed"
                                );
                            } else {
                                tracing::warn!(
                                    peer = %remote_peer,
                                    attempt_id,
                                    %reason,
                                    nat = %nat_type_summary(nat_mapping, &observed_nat_ports),
                                    direct_connections = connection_state.direct_connection_count(remote_peer),
                                    relayed_connections = connection_state.relayed_connection_count(remote_peer),
                                    known_addrs = ?known_addrs,
                                    "hole punch failed"
                                );
                            }
                        }
                        Event::HolePunchAttemptTimeout { peer, attempt_id } => {
                            tracing::warn!(
                                peer = %peer,
                                attempt_id,
                                timeout_secs = HOLE_PUNCH_TIMEOUT.as_secs(),
                                direct_connections = connection_state.direct_connection_count(peer),
                                relayed_connections = connection_state.relayed_connection_count(peer),
                                known_addrs = ?known_addrs_for_peer(&app_state, peer),
                                "hole punch timeout reached without direct upgrade; scheduling retry"
                            );
                        }
                        _ => {}
                    }
                    let old_phase = app_state.phase();
                    let event_label = state_event.to_string();
                    let (new_state, commands) = app_state.transition(state_event);
                    tracing::debug!(event = %event_label, commands = commands.len(), "event processed");
                    log_phase_transition(old_phase, new_state.phase(), commands.len());
                    app_state = new_state;
                    execute_commands(&mut swarm, &commands, &config.exposed, &mut ctx);
                    app_state = arm_hole_punch_for_tunnel_targets(
                        app_state,
                        &mut connection_state,
                        &mut swarm,
                        &config.exposed,
                        &mut ctx,
                    );

                    match app_state.phase() {
                        Phase::ShuttingDown => {
                            tracing::info!("shutting down");
                            break;
                        }
                        Phase::Joining | Phase::Ready => {}
                    }
                }

                match app_state.phase() {
                    Phase::ShuttingDown => break,
                    Phase::Joining | Phase::Ready => {}
                }
            }
            _ = &mut discovery_deadline, if !discovery_timeout_fired => {
                discovery_timeout_fired = true;
                let old_phase = app_state.phase();
                let (new_state, commands) = app_state.transition(Event::DiscoveryTimeout);
                tracing::debug!(event = "DiscoveryTimeout", commands = commands.len(), "event processed");
                log_phase_transition(old_phase, new_state.phase(), commands.len());
                app_state = new_state;
                execute_commands(&mut swarm, &commands, &config.exposed, &mut ctx);
            }
            _ = async {
                match next_reconnect {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending::<()>().await,
                }
            } => {
                let now = tokio::time::Instant::now();
                let due: Vec<PeerId> = pending_reconnects
                    .iter()
                    .filter(|(_, deadline)| **deadline <= now)
                    .map(|(pid, _)| *pid)
                    .collect();

                for peer_id in due {
                    pending_reconnects.remove(&peer_id);
                    if let Some(addr) = backoff.addrs.get(&peer_id) {
                        tracing::info!(%peer_id, %addr, "attempting bootstrap reconnect");
                        match swarm.dial(addr.clone()) {
                            Ok(()) => {}
                            Err(e) => {
                                tracing::warn!(%peer_id, error = %e, "reconnect dial failed");
                                if let Some((_, delay)) = backoff.schedule_reconnect(peer_id) {
                                    pending_reconnects.insert(
                                        peer_id,
                                        tokio::time::Instant::now() + delay,
                                    );
                                }
                            }
                        }
                    }
                }
            }
            _ = async {
                match next_hole_punch_timeout {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending::<()>().await,
                }
            } => {
                let now = tokio::time::Instant::now();
                let due = connection_state.take_due_hole_punch_timeouts(now);

                for (peer, attempt_id) in due {
                    if connection_state.peer_is_relay_only(&peer) {
                        let old_phase = app_state.phase();
                        let (new_state, commands) = app_state.transition(Event::HolePunchAttemptTimeout {
                            peer,
                            attempt_id,
                        });
                        tracing::debug!(
                            event = "HolePunchAttemptTimeout",
                            commands = commands.len(),
                            "event processed"
                        );
                        log_phase_transition(old_phase, new_state.phase(), commands.len());
                        app_state = new_state;
                        execute_commands(&mut swarm, &commands, &config.exposed, &mut ctx);
                    }
                }
            }
            shutdown_result = &mut shutdown_signal => {
                if let Err(error) = shutdown_result {
                    tracing::warn!(error = %error, "shutdown signal listener failed");
                }
                let old_phase = app_state.phase();
                let (new_state, commands) = app_state.transition(Event::ShutdownRequested);
                tracing::debug!(event = "ShutdownRequested", commands = commands.len(), "event processed");
                log_phase_transition(old_phase, new_state.phase(), commands.len());
                app_state = new_state;
                execute_commands(&mut swarm, &commands, &config.exposed, &mut ctx);
                match app_state.phase() {
                    Phase::ShuttingDown => {
                        tracing::info!("shutting down");
                        break;
                    }
                    Phase::Joining | Phase::Ready => {}
                }
            }
        }
    }

    ctx.tunnel_registry
        .shutdown_all(Duration::from_secs(5))
        .await;

    Ok(())
}

// ─── Helpers ────────────────────────────────────────────────────────────────

fn extract_peer_id(addr: &Multiaddr) -> Option<PeerId> {
    addr.iter().find_map(|proto| match proto {
        libp2p::multiaddr::Protocol::P2p(peer_id) => Some(peer_id),
        _ => None,
    })
}

fn extract_quic_udp_port(addr: &Multiaddr) -> Option<u16> {
    addr.iter().find_map(|p| match p {
        libp2p::multiaddr::Protocol::Udp(port) => Some(port),
        _ => None,
    })
}

fn log_connection_opened(
    peer_id: &PeerId,
    connection_id: &ConnectionId,
    endpoint: &libp2p::core::ConnectedPoint,
    relayed: bool,
) {
    match endpoint {
        libp2p::core::ConnectedPoint::Dialer { address, .. } => {
            tracing::debug!(
                peer = %peer_id,
                connection_id = ?connection_id,
                relayed,
                direction = "outbound",
                remote_addr = %address,
                "connection opened"
            );
        }
        libp2p::core::ConnectedPoint::Listener {
            local_addr,
            send_back_addr,
        } => {
            tracing::debug!(
                peer = %peer_id,
                connection_id = ?connection_id,
                relayed,
                direction = "inbound",
                local_addr = %local_addr,
                remote_addr = %send_back_addr,
                "connection opened"
            );
        }
    }
}

fn endpoint_is_relayed(endpoint: &libp2p::core::ConnectedPoint) -> bool {
    fn has_circuit(addr: &Multiaddr) -> bool {
        addr.iter()
            .any(|p| matches!(p, libp2p::multiaddr::Protocol::P2pCircuit))
    }

    match endpoint {
        libp2p::core::ConnectedPoint::Dialer { address, .. } => has_circuit(address),
        libp2p::core::ConnectedPoint::Listener {
            local_addr,
            send_back_addr,
        } => has_circuit(local_addr) || has_circuit(send_back_addr),
    }
}

fn known_addrs_for_peer(app_state: &AppState, peer: &PeerId) -> Vec<String> {
    app_state
        .peer
        .known_peers
        .get(peer)
        .map(|addrs| addrs.iter().map(ToString::to_string).collect())
        .unwrap_or_default()
}

fn log_phase_transition(old: Phase, new: Phase, commands: usize) {
    match old == new {
        true => {}
        false => {
            tracing::info!(
                from = %old,
                to = %new,
                commands,
                "phase transition"
            );
        }
    }
}

fn arm_hole_punch_for_tunnel_targets(
    mut app_state: AppState,
    connection_state: &mut ConnectionStateMachine,
    swarm: &mut libp2p::Swarm<Behaviour>,
    exposed: &[ExposedService],
    ctx: &mut ExecutionContext,
) -> AppState {
    if matches!(app_state.phase(), Phase::ShuttingDown) {
        return app_state;
    }
    let target_peers = app_state.tunnel.holepunch_target_peers();
    for peer in target_peers {
        let Some(attempt_id) = connection_state.start_hole_punch_attempt(peer, HOLE_PUNCH_TIMEOUT)
        else {
            continue;
        };
        let known_addrs = known_addrs_for_peer(&app_state, &peer);
        let old_phase = app_state.phase();
        let (new_state, commands) =
            app_state.transition(Event::HolePunchAttemptStarted { peer, attempt_id });
        tracing::info!(
            peer = %peer,
            attempt_id,
            timeout_secs = HOLE_PUNCH_TIMEOUT.as_secs(),
            known_addrs = ?known_addrs,
            "hole punch started"
        );
        tracing::debug!(
            event = "HolePunchAttemptStarted",
            commands = commands.len(),
            "event processed"
        );
        log_phase_transition(old_phase, new_state.phase(), commands.len());
        app_state = new_state;
        execute_commands(swarm, &commands, exposed, ctx);
    }
    app_state
}

#[derive(Debug, Clone, Copy, Default)]
struct HolePunchErrorFlags {
    has_handshake_timeout: bool,
    has_unsupported_bare_p2p: bool,
    has_pending_abort: bool,
}

fn hole_punch_error_flags(error: &impl std::fmt::Display) -> HolePunchErrorFlags {
    let error_text = error.to_string();
    HolePunchErrorFlags {
        has_handshake_timeout: error_text.contains("Handshake with the remote timed out"),
        has_unsupported_bare_p2p: error_text.contains("Multiaddr is not supported: /p2p/"),
        has_pending_abort: error_text.contains("Pending connection attempt has been aborted"),
    }
}

fn nat_type_summary(
    stun_result: crate::nat_probe::NatMapping,
    observations: &HashMap<IpAddr, HashSet<u16>>,
) -> String {
    match stun_result {
        crate::nat_probe::NatMapping::EndpointIndependent => "cone NAT (STUN-verified)".to_string(),
        crate::nat_probe::NatMapping::AddressDependent => {
            "symmetric NAT (STUN-verified)".to_string()
        }
        crate::nat_probe::NatMapping::Unknown => {
            let max_ports_per_ip = observations.values().map(|p| p.len()).max().unwrap_or(0);
            match max_ports_per_ip {
                0 => "no observations".to_string(),
                1 => "undetermined (single observer per IP)".to_string(),
                _ => "symmetric NAT likely (multiple ports per IP)".to_string(),
            }
        }
    }
}
