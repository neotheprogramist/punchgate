mod backoff;
mod execute;
mod translate;

use std::{
    collections::{HashMap, HashSet},
    net::IpAddr,
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result};
use futures::StreamExt;
use libp2p::{Multiaddr, PeerId, SwarmBuilder, noise, swarm::SwarmEvent, yamux};

use self::{
    backoff::ReconnectBackoff,
    execute::{ExecutionContext, execute_commands, publish_services},
    translate::translate_swarm_event,
};
use crate::{
    behaviour::Behaviour,
    external_addr,
    protocol::{
        self, DISCOVERY_TIMEOUT, HOLEPUNCH_RETRY_INTERVAL, HOLEPUNCH_TIMEOUT,
        IDLE_CONNECTION_TIMEOUT,
    },
    shutdown,
    specs::{ExposedService, ServiceAddr, TunnelByNameSpec, TunnelSpec},
    state::{AppState, Command, Event, NatStatus, PeerState, Phase, TunnelState},
    traits::MealyMachine,
    tunnel::TunnelRegistry,
    types::ServiceName,
};

// ─── Main entry ─────────────────────────────────────────────────────────────

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
                        "hole-punching not viable, all tunnels will use relay transport"
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

    if config.bootstrap_addrs.is_empty() {
        let mut probe_control = swarm.behaviour().stream.new_control();
        let probe_incoming = probe_control
            .accept(protocol::nat_probe_protocol())
            .map_err(|_| anyhow::anyhow!("NAT probe protocol already registered"))?;
        tokio::spawn(nat_probe_accept_loop(probe_incoming));
    }

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

    let filtering_probe_control = swarm.behaviour().stream.new_control();

    let mut ctx = ExecutionContext {
        active_bootstrap_query: None,
        kad_tunnel_queries: HashMap::new(),
        kad_service_queries: HashMap::new(),
        stream_control,
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

        let old_phase = app_state.phase();
        let (new_state, commands) = app_state.transition(Event::NatMappingDetected(nat_mapping));
        log_phase_transition(old_phase, new_state.phase(), commands.len());
        app_state = new_state;
        execute_commands(&mut swarm, &commands, &config.exposed, &mut ctx);
    }

    let discovery_deadline = tokio::time::sleep(DISCOVERY_TIMEOUT);
    tokio::pin!(discovery_deadline);
    let mut discovery_timeout_fired = false;

    let mut republish_interval = tokio::time::interval(protocol::SERVICE_REPUBLISH_INTERVAL);
    republish_interval.tick().await;

    let mut backoff = ReconnectBackoff::new(&bootstrap_peers_with_addrs);
    let mut pending_reconnects: HashMap<PeerId, tokio::time::Instant> = HashMap::new();
    let mut holepunch_deadlines: HashMap<PeerId, tokio::time::Instant> = HashMap::new();
    let mut holepunch_retries: HashMap<PeerId, tokio::time::Instant> = HashMap::new();
    let mut observed_nat_ports: HashMap<IpAddr, HashSet<u16>> = HashMap::new();
    let mut relayed_connections: HashSet<PeerId> = HashSet::new();
    let mut port_mapping_handle: Option<
        tokio::task::JoinHandle<Option<crate::port_mapping::PortMapping>>,
    > = None;
    let mut port_mapping_spawned = false;

    let shutdown_signal = shutdown::shutdown_signal();
    tokio::pin!(shutdown_signal);

    loop {
        let next_reconnect = pending_reconnects.values().min().copied();
        let next_holepunch = holepunch_deadlines.values().min().copied();
        let next_holepunch_retry = holepunch_retries.values().min().copied();

        tokio::select! {
            event = swarm.select_next_some() => {
                if let SwarmEvent::NewListenAddr { address, .. } = &event {
                    tracing::info!("listening on {address}");
                }

                if let SwarmEvent::NewListenAddr { address, .. } = &event
                    && !port_mapping_spawned
                    && !config.bootstrap_addrs.is_empty()
                    && let Some(port) = extract_quic_udp_port(address)
                {
                    port_mapping_spawned = true;
                    let handle = tokio::spawn(crate::port_mapping::acquire_port_mapping(port));
                    port_mapping_handle = Some(handle);
                    tracing::info!(port, "spawned port mapping probe");
                }

                if let SwarmEvent::ConnectionEstablished { peer_id, endpoint, .. } = &event {
                    tracing::info!(peer = %peer_id, endpoint = %endpoint.get_remote_address(), "connection opened");
                    let addr = match endpoint {
                        libp2p::core::ConnectedPoint::Dialer { address, .. } => address,
                        libp2p::core::ConnectedPoint::Listener { send_back_addr, .. } => send_back_addr,
                    };
                    if addr.iter().any(|p| matches!(p, libp2p::multiaddr::Protocol::P2pCircuit)) {
                        relayed_connections.insert(*peer_id);
                    } else {
                        relayed_connections.remove(peer_id);
                    }
                }
                if let SwarmEvent::ConnectionClosed { peer_id, num_established, .. } = &event {
                    tracing::info!(peer = %peer_id, remaining = num_established, "connection closed");
                    if *num_established == 0 {
                        relayed_connections.remove(peer_id);
                    }
                }

                if let SwarmEvent::ConnectionEstablished { peer_id, .. } = &event
                    && bootstrap_peer_ids.contains(peer_id)
                {
                    backoff.reset(peer_id);
                    pending_reconnects.remove(peer_id);
                }

                if let SwarmEvent::ConnectionClosed { peer_id, num_established: 0, .. } = &event
                    && bootstrap_peer_ids.contains(peer_id)
                    && let Some((_, delay)) = backoff.schedule_reconnect(*peer_id)
                {
                    let deadline = tokio::time::Instant::now() + delay;
                    pending_reconnects.insert(*peer_id, deadline);
                    tracing::info!(
                        %peer_id,
                        delay_secs = delay.as_secs(),
                        "scheduling bootstrap reconnect"
                    );
                }

                if let SwarmEvent::OutgoingConnectionError { peer_id: Some(peer_id), .. } = &event
                    && bootstrap_peer_ids.contains(peer_id)
                    && !swarm.is_connected(peer_id)
                    && !pending_reconnects.contains_key(peer_id)
                    && let Some((_, delay)) = backoff.schedule_reconnect(*peer_id)
                {
                    pending_reconnects.insert(*peer_id, tokio::time::Instant::now() + delay);
                    tracing::info!(
                        %peer_id,
                        delay_secs = delay.as_secs(),
                        "rescheduling bootstrap reconnect after failed dial"
                    );
                }

                if let SwarmEvent::Dialing { peer_id: Some(peer_id), .. } = &event
                    && holepunch_deadlines.contains_key(peer_id)
                {
                    tracing::info!(
                        peer = %peer_id,
                        "hole-punch dial attempt in progress"
                    );
                }

                if let SwarmEvent::OutgoingConnectionError { peer_id: Some(peer_id), error, .. } = &event
                    && holepunch_deadlines.contains_key(peer_id)
                {
                    tracing::info!(
                        peer = %peer_id,
                        error = %error,
                        "hole-punch dial attempt failed"
                    );
                }

                if let SwarmEvent::NewExternalAddrCandidate { address } = &event {
                    if external_addr::is_valid_external_candidate(address) {
                        tracing::info!(%address, "new external address candidate (confirming)");
                        swarm.add_external_address(address.clone());
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
                            tracing::info!("port mapping unavailable, relay path will handle connectivity");
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "port mapping task panicked");
                        }
                    }
                }

                // Translate swarm event → domain events, then feed through state machine
                let events = translate_swarm_event(
                    &event,
                    &bootstrap_peer_ids,
                    ctx.active_bootstrap_query,
                    &mut ctx.kad_tunnel_queries,
                    &mut ctx.kad_service_queries,
                    |peer| swarm.is_connected(peer),
                );

                for state_event in events {
                    // Bootstrap server: confirm observed addresses from Identify
                    // (first node has nobody to AutoNAT it)
                    if bootstrap_peer_ids.is_empty()
                        && let Event::PeerIdentified { observed_addr, .. } = &state_event
                    {
                        swarm.add_external_address(observed_addr.clone());
                    }

                    // NAT type detection: track external port observations per IP
                    // Skip relay-connected peers — their observed_addr reports the relay's address
                    if let Event::PeerIdentified { peer, observed_addr, .. } = &state_event
                        && !relayed_connections.contains(peer)
                        && let Some((ip, port)) = external_addr::extract_public_ip_port(observed_addr)
                    {
                        let ports = observed_nat_ports.entry(ip).or_default();
                        if ports.insert(port) {
                            match ports.len() {
                                1 => tracing::info!(
                                    observer = %peer,
                                    observed_ip = %ip,
                                    observed_port = port,
                                    "NAT mapping observed"
                                ),
                                _ => tracing::warn!(
                                    observer = %peer,
                                    observed_ip = %ip,
                                    observed_ports = ?ports,
                                    "symmetric NAT likely — multiple external ports for same IP, hole-punching will fail"
                                ),
                            }
                        }
                    }

                    match &state_event {
                        Event::RelayReservationAccepted { relay_peer } => {
                            tracing::info!(%relay_peer, "relay reservation accepted");
                            if nat_mapping == crate::nat_probe::NatMapping::EndpointIndependent
                                && let Some(&stun_server) = crate::nat_probe::DEFAULT_STUN_SERVERS.first()
                            {
                                let probe_ctl = filtering_probe_control.clone();
                                let bootstrap = *relay_peer;
                                tokio::spawn(async move {
                                    let filtering = crate::nat_probe::probe_filtering(
                                        probe_ctl,
                                        bootstrap,
                                        stun_server,
                                    ).await;
                                    tracing::info!(
                                        %filtering,
                                        %bootstrap,
                                        "NAT filtering probe result"
                                    );
                                });
                            }
                        }
                        Event::HolePunchSucceeded { remote_peer } => {
                            tracing::info!(peer = %remote_peer, "hole punch succeeded");
                            holepunch_deadlines.remove(remote_peer);
                            holepunch_retries.remove(remote_peer);
                        }
                        Event::HolePunchFailed { remote_peer, reason } => {
                            tracing::warn!(
                                peer = %remote_peer,
                                %reason,
                                nat = %nat_type_summary(nat_mapping, &observed_nat_ports),
                                "hole punch failed"
                            );
                            holepunch_deadlines.remove(remote_peer);
                        }
                        Event::TunnelPeerConnected { peer, relayed: false } => {
                            holepunch_deadlines.remove(peer);
                            holepunch_retries.remove(peer);
                        }
                        Event::ConnectionLost { peer, remaining_connections: 0 } => {
                            holepunch_deadlines.remove(peer);
                            holepunch_retries.remove(peer);
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

                    for cmd in &commands {
                        if let Command::AwaitHolePunch { peer } = cmd {
                            holepunch_deadlines.entry(*peer)
                                .or_insert(tokio::time::Instant::now() + HOLEPUNCH_TIMEOUT);
                            tracing::info!(
                                %peer,
                                timeout_secs = HOLEPUNCH_TIMEOUT.as_secs(),
                                "hole-punch timer started"
                            );
                        }
                    }

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
            _ = republish_interval.tick(), if app_state.phase() == Phase::Ready => {
                publish_services(&mut swarm, &config.exposed);
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
                match next_holepunch {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending::<()>().await,
                }
            } => {
                let now = tokio::time::Instant::now();
                let due: Vec<PeerId> = holepunch_deadlines
                    .iter()
                    .filter(|(_, deadline)| **deadline <= now)
                    .map(|(pid, _)| *pid)
                    .collect();

                for peer in due {
                    holepunch_deadlines.remove(&peer);
                    tracing::info!(
                        %peer,
                        nat = %nat_type_summary(nat_mapping, &observed_nat_ports),
                        "hole punch timeout, falling back to relay"
                    );
                    let old_phase = app_state.phase();
                    let (new_state, commands) = app_state.transition(Event::HolePunchTimeout { peer });
                    tracing::debug!(event = "HolePunchTimeout", commands = commands.len(), "event processed");
                    log_phase_transition(old_phase, new_state.phase(), commands.len());
                    app_state = new_state;
                    execute_commands(&mut swarm, &commands, &config.exposed, &mut ctx);

                    holepunch_retries.entry(peer)
                        .or_insert(tokio::time::Instant::now() + HOLEPUNCH_RETRY_INTERVAL);
                }
            }
            _ = async {
                match next_holepunch_retry {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending::<()>().await,
                }
            } => {
                let now = tokio::time::Instant::now();
                let due: Vec<PeerId> = holepunch_retries
                    .iter()
                    .filter(|(_, deadline)| **deadline <= now)
                    .map(|(pid, _)| *pid)
                    .collect();

                for peer in due {
                    holepunch_retries.remove(&peer);
                    tracing::info!(%peer, "retrying hole-punch via direct dial");
                    let old_phase = app_state.phase();
                    let (new_state, commands) = app_state.transition(Event::HolePunchRetryTick { peer });
                    tracing::debug!(event = "HolePunchRetryTick", commands = commands.len(), "event processed");
                    log_phase_transition(old_phase, new_state.phase(), commands.len());
                    app_state = new_state;
                    execute_commands(&mut swarm, &commands, &config.exposed, &mut ctx);

                    if commands.iter().any(|c| matches!(c, Command::RetryDirectDial { .. })) {
                        holepunch_retries.insert(peer, tokio::time::Instant::now() + HOLEPUNCH_RETRY_INTERVAL);
                    }
                }
            }
            _ = &mut shutdown_signal => {
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

// ─── NAT probe handler (bootstrap only) ─────────────────────────────────────

async fn nat_probe_accept_loop(mut incoming: libp2p_stream::IncomingStreams) {
    while let Some((peer_id, mut stream)) = incoming.next().await {
        tokio::spawn(async move {
            match handle_nat_probe(peer_id, &mut stream).await {
                Ok(()) => {}
                Err(e) => tracing::warn!(%peer_id, error = %e, "NAT probe handler failed"),
            }
        });
    }
}

async fn handle_nat_probe(
    peer_id: libp2p::PeerId,
    stream: &mut libp2p::Stream,
) -> anyhow::Result<()> {
    use futures::io::{AsyncReadExt, AsyncWriteExt};

    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await?;
    // Infallible: u32 always fits in usize on all Rust std-supported platforms (>= 32-bit)
    let len = usize::try_from(u32::from_be_bytes(len_buf))
        .expect("u32 fits in usize on all supported platforms");
    const MAX_MESSAGE_SIZE: usize = 65536;
    anyhow::ensure!(
        len <= MAX_MESSAGE_SIZE,
        "NAT probe request too large: {len} bytes"
    );

    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf).await?;
    let request: crate::nat_probe_protocol::NatProbeRequest = serde_json::from_slice(&buf)?;

    let socket = tokio::net::UdpSocket::bind("0.0.0.0:0").await?;
    socket
        .send_to(b"punchgate-nat-probe", request.mapped_addr)
        .await?;

    tracing::info!(
        %peer_id,
        mapped_addr = %request.mapped_addr,
        "sent NAT filtering probe"
    );

    let response = crate::nat_probe_protocol::NatProbeResponse { probe_sent: true };
    let data = serde_json::to_vec(&response)?;
    let response_len =
        u32::try_from(data.len()).context("NAT probe response too large for u32 length prefix")?;
    stream.write_all(&response_len.to_be_bytes()).await?;
    stream.write_all(&data).await?;
    stream.flush().await?;

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
