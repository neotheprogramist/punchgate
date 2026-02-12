use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use anyhow::Result;
use futures::StreamExt;
#[cfg(feature = "mdns")]
use libp2p::mdns;
use libp2p::{
    Multiaddr, PeerId, SwarmBuilder, autonat, identify, kad, noise, ping, relay, swarm::SwarmEvent,
    yamux,
};

use crate::{
    behaviour::{Behaviour, BehaviourEvent},
    protocol::{self, DISCOVERY_TIMEOUT, IDLE_CONNECTION_TIMEOUT},
    service::{self, ExposedService, ServiceAddr, TunnelByNameSpec},
    shutdown,
    state::{Command, Event, NatStatus, PeerState, Phase},
    tunnel::{self, TunnelRegistry, TunnelSpec},
};

// ─── Reconnection backoff ───────────────────────────────────────────────────

const BACKOFF_INITIAL: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(60);

struct ReconnectBackoff {
    delays: HashMap<PeerId, Duration>,
    addrs: HashMap<PeerId, Multiaddr>,
}

impl ReconnectBackoff {
    fn new(bootstrap: &[(PeerId, Multiaddr)]) -> Self {
        Self {
            delays: HashMap::new(),
            addrs: bootstrap.iter().cloned().collect(),
        }
    }

    fn schedule_reconnect(&mut self, peer: PeerId) -> Option<(Multiaddr, Duration)> {
        let addr = self.addrs.get(&peer)?;
        let delay = self.delays.entry(peer).or_insert(BACKOFF_INITIAL);
        let current = *delay;
        *delay = delay.saturating_mul(2).min(BACKOFF_MAX);
        Some((addr.clone(), current))
    }

    fn reset(&mut self, peer: &PeerId) {
        self.delays.remove(peer);
    }
}

// ─── Main entry ─────────────────────────────────────────────────────────────

pub struct NodeConfig {
    pub identity_path: PathBuf,
    pub listen_addrs: Vec<Multiaddr>,
    pub bootstrap_addrs: Vec<Multiaddr>,
    pub exposed: Vec<ExposedService>,
    pub tunnel_specs: Vec<TunnelSpec>,
    pub tunnel_by_name_specs: Vec<TunnelByNameSpec>,
}

/// Run the unified peer node.
pub async fn run(config: NodeConfig) -> Result<()> {
    let keypair = crate::identity::load_or_generate(&config.identity_path)?;
    let local_peer_id = PeerId::from(keypair.public());
    tracing::info!(%local_peer_id, "starting punchgate node");

    let services: Arc<HashMap<String, ServiceAddr>> = Arc::new(
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

    let mut swarm = SwarmBuilder::with_existing_identity(keypair)
        .with_tokio()
        .with_tcp(
            Default::default(),
            noise::Config::new,
            yamux::Config::default,
        )?
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
    tokio::spawn(tunnel::accept_loop(incoming, services.clone()));

    // Build deferred pending tunnels from typed specs (spawned reactively after connection)
    let mut tunnel_registry = TunnelRegistry::new();
    let mut pending_tunnels: HashMap<PeerId, Vec<(String, ServiceAddr)>> = HashMap::new();
    for spec in &config.tunnel_specs {
        pending_tunnels
            .entry(spec.remote_peer)
            .or_default()
            .push((spec.service_name.clone(), spec.bind_addr.clone()));
    }

    let mut kad_tunnel_queries: HashMap<kad::QueryId, PeerId> = HashMap::new();
    let mut tunnel_peers_dialing: HashSet<PeerId> = HashSet::new();

    // Pending service-name-based tunnels (not yet resolved to a peer)
    let mut pending_service_tunnels: HashMap<String, Vec<ServiceAddr>> = HashMap::new();
    for spec in &config.tunnel_by_name_specs {
        pending_service_tunnels
            .entry(spec.service_name.clone())
            .or_default()
            .push(spec.bind_addr.clone());
    }
    // Track GetProviders query IDs → service names
    let mut kad_service_queries: HashMap<kad::QueryId, String> = HashMap::new();

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
    let mut active_bootstrap_query: Option<kad::QueryId> = None;

    if config.bootstrap_addrs.is_empty() {
        let old_phase = peer_state.phase;
        let (new_state, commands) = peer_state.transition(Event::NoBootstrapPeers);
        log_phase_transition(
            "NoBootstrapPeers",
            old_phase,
            new_state.phase,
            commands.len(),
        );
        peer_state = new_state;
        execute_commands(
            &mut swarm,
            &commands,
            &config.exposed,
            &mut active_bootstrap_query,
        );
    }

    let discovery_deadline = tokio::time::sleep(DISCOVERY_TIMEOUT);
    tokio::pin!(discovery_deadline);
    let mut discovery_timeout_fired = false;

    let mut republish_interval = tokio::time::interval(protocol::SERVICE_REPUBLISH_INTERVAL);
    republish_interval.tick().await; // skip first immediate tick

    let mut backoff = ReconnectBackoff::new(&bootstrap_peers_with_addrs);
    let mut pending_reconnects: HashMap<PeerId, tokio::time::Instant> = HashMap::new();

    let shutdown_signal = shutdown::shutdown_signal();
    tokio::pin!(shutdown_signal);

    loop {
        let next_reconnect = pending_reconnects.values().min().copied();

        tokio::select! {
            event = swarm.select_next_some() => {
                // Log all connection events for observability
                if let SwarmEvent::ConnectionEstablished { peer_id, endpoint, .. } = &event {
                    tracing::info!(peer = %peer_id, endpoint = %endpoint.get_remote_address(), "connection opened");
                }
                if let SwarmEvent::ConnectionClosed { peer_id, num_established, .. } = &event {
                    tracing::info!(peer = %peer_id, remaining = num_established, "connection closed");
                }

                // Check if a bootstrap peer reconnected
                if let SwarmEvent::ConnectionEstablished { peer_id, .. } = &event
                    && bootstrap_peer_ids.contains(peer_id)
                {
                    backoff.reset(peer_id);
                    pending_reconnects.remove(peer_id);
                }

                // Check if a bootstrap peer disconnected — schedule reconnect
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

                // Reschedule reconnect when async dial to bootstrap peer fails
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

                // Auto-confirm external address candidates discovered via Identify/AutoNAT
                if let SwarmEvent::NewExternalAddrCandidate { address } = &event {
                    swarm.add_external_address(address.clone());
                }

                // Handle Kademlia GetClosestPeers results for tunnel target lookups
                if let SwarmEvent::Behaviour(BehaviourEvent::Kademlia(
                    kad::Event::OutboundQueryProgressed {
                        id,
                        result: kad::QueryResult::GetClosestPeers(..),
                        step,
                        ..
                    }
                )) = &event
                    && step.last
                    && let Some(target_peer) = kad_tunnel_queries.remove(id)
                {
                    match swarm.is_connected(&target_peer) {
                        true => {
                            if let Some(specs) = pending_tunnels.remove(&target_peer) {
                                tracing::info!(%target_peer, "tunnel target already connected, spawning tunnels");
                                spawn_tunnels(&specs, target_peer, &stream_control, &mut tunnel_registry);
                            }
                        }
                        false => match swarm.dial(target_peer) {
                            Ok(()) => {
                                tunnel_peers_dialing.insert(target_peer);
                                tracing::info!(%target_peer, "dialing tunnel target after DHT lookup");
                            }
                            Err(e) => {
                                tracing::error!(%target_peer, error = %e, "failed to dial tunnel target");
                                pending_tunnels.remove(&target_peer);
                            }
                        },
                    }
                }

                // Handle Kademlia GetProviders results for service-name discovery
                if let SwarmEvent::Behaviour(BehaviourEvent::Kademlia(
                    kad::Event::OutboundQueryProgressed {
                        id,
                        result: kad::QueryResult::GetProviders(result),
                        ..
                    }
                )) = &event
                    && kad_service_queries.contains_key(id)
                {
                    match result {
                        Ok(kad::GetProvidersOk::FoundProviders { providers, .. }) => {
                            if let Some(first_provider) = providers.iter().next()
                                && let Some(service_name) = kad_service_queries.remove(id)
                                && let Some(bind_addrs) = pending_service_tunnels.remove(&service_name)
                            {
                                let specs: Vec<(String, ServiceAddr)> = bind_addrs
                                    .into_iter()
                                    .map(|addr| (service_name.clone(), addr))
                                    .collect();

                                match swarm.is_connected(first_provider) {
                                    true => {
                                        tracing::info!(
                                            %service_name, %first_provider,
                                            "service provider already connected, spawning tunnels"
                                        );
                                        spawn_tunnels(&specs, *first_provider, &stream_control, &mut tunnel_registry);
                                    }
                                    false => {
                                        pending_tunnels
                                            .entry(*first_provider)
                                            .or_default()
                                            .extend(specs);
                                        let query_id = swarm.behaviour_mut().kademlia
                                            .get_closest_peers(*first_provider);
                                        kad_tunnel_queries.insert(query_id, *first_provider);
                                        tracing::info!(
                                            %service_name, %first_provider,
                                            "service provider discovered, initiating connection"
                                        );
                                    }
                                }
                            }
                        }
                        Ok(kad::GetProvidersOk::FinishedWithNoAdditionalRecord { .. }) => {
                            if let Some(service_name) = kad_service_queries.remove(id)
                                && pending_service_tunnels.contains_key(&service_name)
                            {
                                tracing::warn!(%service_name, "DHT lookup finished with no providers");
                            }
                        }
                        Err(e) => {
                            if let Some(service_name) = kad_service_queries.remove(id) {
                                pending_service_tunnels.remove(&service_name);
                                tracing::error!(%service_name, error = %e, "service provider lookup failed");
                            }
                        }
                    }
                }

                // Handle ConnectionEstablished for tunnel targets
                if let SwarmEvent::ConnectionEstablished { peer_id, endpoint, .. } = &event
                    && pending_tunnels.contains_key(peer_id)
                {
                    tunnel_peers_dialing.remove(peer_id);
                    match is_relayed(endpoint) {
                        true => {
                            tracing::info!(%peer_id, "connected to tunnel target via relay, waiting for DCUtR hole punch");
                        }
                        false => {
                            if let Some(specs) = pending_tunnels.remove(peer_id) {
                                spawn_tunnels(&specs, *peer_id, &stream_control, &mut tunnel_registry);
                            }
                        }
                    }
                }

                if let Some(state_event) = translate_swarm_event(
                    &event,
                    &bootstrap_peer_ids,
                    active_bootstrap_query,
                ) {
                    match &state_event {
                        Event::HolePunchSucceeded { remote_peer } => {
                            if let Some(specs) = pending_tunnels.remove(remote_peer) {
                                tracing::info!(%remote_peer, "hole punch succeeded, spawning tunnel");
                                spawn_tunnels(&specs, *remote_peer, &stream_control, &mut tunnel_registry);
                            }
                        }
                        Event::HolePunchFailed { remote_peer, reason } => {
                            if pending_tunnels.remove(remote_peer).is_some() {
                                tracing::error!(%remote_peer, %reason, "hole punch failed — tunnel cannot be established (direct connection required)");
                            }
                        }
                        Event::ListeningOn { .. }
                        | Event::BootstrapConnected { .. }
                        | Event::ShutdownRequested
                        | Event::KademliaBootstrapOk
                        | Event::KademliaBootstrapFailed { .. }
                        | Event::MdnsDiscovered { .. }
                        | Event::MdnsExpired { .. }
                        | Event::PeerIdentified { .. }
                        | Event::NatStatusChanged(_)
                        | Event::DiscoveryTimeout
                        | Event::RelayReservationAccepted { .. }
                        | Event::RelayReservationFailed { .. }
                        | Event::ConnectionLost { .. }
                        | Event::ServiceProvidersFound { .. }
                        | Event::ServiceLookupFailed { .. }
                        | Event::NoBootstrapPeers
                        | Event::ExternalAddrConfirmed { .. }
                        | Event::ExternalAddrExpired { .. } => {}
                    }

                    let old_phase = peer_state.phase;
                    let event_label = event_name(&state_event);
                    let (new_state, commands) = peer_state.transition(state_event);
                    tracing::debug!(event = event_label, commands = commands.len(), "event processed");
                    log_phase_transition(event_label, old_phase, new_state.phase, commands.len());
                    peer_state = new_state;
                    execute_commands(
                        &mut swarm,
                        &commands,
                        &config.exposed,
                        &mut active_bootstrap_query,
                    );

                    // Initiate DHT lookups when transitioning to Participating
                    if let (Phase::Initializing | Phase::Discovering | Phase::ShuttingDown, Phase::Participating) = (old_phase, peer_state.phase) {
                        initiate_tunnel_lookups(&mut swarm, &pending_tunnels, &mut kad_tunnel_queries);
                        initiate_service_lookups(&mut swarm, &pending_service_tunnels, &mut kad_service_queries);
                    }

                    match peer_state.phase {
                        Phase::ShuttingDown => {
                            tracing::info!("shutting down");
                            break;
                        }
                        Phase::Initializing | Phase::Discovering | Phase::Participating => {}
                    }
                }
            }
            _ = &mut discovery_deadline, if !discovery_timeout_fired => {
                discovery_timeout_fired = true;
                let old_phase = peer_state.phase;
                let (new_state, commands) = peer_state.transition(Event::DiscoveryTimeout);
                tracing::debug!(event = "DiscoveryTimeout", commands = commands.len(), "event processed");
                log_phase_transition("DiscoveryTimeout", old_phase, new_state.phase, commands.len());
                peer_state = new_state;
                execute_commands(&mut swarm, &commands, &config.exposed, &mut active_bootstrap_query);

                if let (Phase::Initializing | Phase::Discovering | Phase::ShuttingDown, Phase::Participating) = (old_phase, peer_state.phase) {
                    initiate_tunnel_lookups(&mut swarm, &pending_tunnels, &mut kad_tunnel_queries);
                    initiate_service_lookups(&mut swarm, &pending_service_tunnels, &mut kad_service_queries);
                }
            }
            _ = republish_interval.tick(), if peer_state.phase == Phase::Participating => {
                publish_services(&mut swarm, &config.exposed);
            }
            _ = async {
                match next_reconnect {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    // No pending reconnects — sleep forever (will be cancelled)
                    None => std::future::pending::<()>().await,
                }
            } => {
                // Fire all due reconnects
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
            _ = &mut shutdown_signal => {
                let old_phase = peer_state.phase;
                let (new_state, commands) = peer_state.transition(Event::ShutdownRequested);
                tracing::debug!(event = "ShutdownRequested", commands = commands.len(), "event processed");
                log_phase_transition("ShutdownRequested", old_phase, new_state.phase, commands.len());
                peer_state = new_state;
                execute_commands(&mut swarm, &commands, &config.exposed, &mut active_bootstrap_query);
                match peer_state.phase {
                    Phase::ShuttingDown => {
                        tracing::info!("shutting down");
                        break;
                    }
                    Phase::Initializing | Phase::Discovering | Phase::Participating => {}
                }
            }
        }
    }

    tunnel_registry.shutdown_all(Duration::from_secs(5)).await;

    Ok(())
}

// ─── Event translation ──────────────────────────────────────────────────────

fn translate_swarm_event(
    event: &SwarmEvent<BehaviourEvent>,
    bootstrap_peers: &HashSet<PeerId>,
    active_bootstrap_query: Option<kad::QueryId>,
) -> Option<Event> {
    match event {
        SwarmEvent::NewListenAddr { address, .. } => Some(Event::ListeningOn {
            addr: address.clone(),
        }),

        SwarmEvent::ConnectionEstablished {
            peer_id, endpoint, ..
        } => match bootstrap_peers.contains(peer_id) {
            true => Some(Event::BootstrapConnected {
                peer: *peer_id,
                addr: endpoint.get_remote_address().clone(),
            }),
            false => None,
        },

        SwarmEvent::ConnectionClosed {
            peer_id,
            num_established,
            ..
        } => Some(Event::ConnectionLost {
            peer: *peer_id,
            remaining_connections: *num_established,
        }),

        SwarmEvent::Behaviour(behaviour_event) => {
            translate_behaviour_event(behaviour_event, active_bootstrap_query)
        }

        SwarmEvent::ListenerError { error, .. } => {
            tracing::warn!(error = %error, "listener error");
            None
        }
        SwarmEvent::ListenerClosed { reason, .. } => {
            tracing::info!(reason = ?reason, "listener closed");
            None
        }
        SwarmEvent::OutgoingConnectionError { peer_id, error, .. } => {
            tracing::warn!(peer = ?peer_id, error = %error, "outgoing connection error");
            None
        }

        SwarmEvent::ExternalAddrConfirmed { address } => Some(Event::ExternalAddrConfirmed {
            addr: address.clone(),
        }),

        SwarmEvent::ExternalAddrExpired { address } => Some(Event::ExternalAddrExpired {
            addr: address.clone(),
        }),

        SwarmEvent::IncomingConnection { .. }
        | SwarmEvent::IncomingConnectionError { .. }
        | SwarmEvent::ExpiredListenAddr { .. }
        | SwarmEvent::Dialing { .. }
        | SwarmEvent::NewExternalAddrCandidate { .. }
        | SwarmEvent::NewExternalAddrOfPeer { .. } => None,

        // SwarmEvent is #[non_exhaustive] — wildcard required for forward compatibility
        _ => None,
    }
}

fn translate_behaviour_event(
    event: &BehaviourEvent,
    active_bootstrap_query: Option<kad::QueryId>,
) -> Option<Event> {
    match event {
        BehaviourEvent::Kademlia(kad::Event::OutboundQueryProgressed {
            id,
            result: kad::QueryResult::Bootstrap(result),
            step,
            ..
        }) => {
            match is_active_bootstrap_result(*id, active_bootstrap_query, step.last) {
                true => {}
                false => return None,
            }
            match result {
                Ok(_) => Some(Event::KademliaBootstrapOk),
                Err(e) => Some(Event::KademliaBootstrapFailed {
                    reason: e.to_string(),
                }),
            }
        }
        // kad::Event is #[non_exhaustive] — wildcard covers other query types
        BehaviourEvent::Kademlia(_) => None,

        #[cfg(feature = "mdns")]
        BehaviourEvent::Mdns(mdns::Event::Discovered(list)) => Some(Event::MdnsDiscovered {
            peers: list.clone(),
        }),
        #[cfg(feature = "mdns")]
        BehaviourEvent::Mdns(mdns::Event::Expired(list)) => Some(Event::MdnsExpired {
            peers: list.clone(),
        }),

        BehaviourEvent::Identify(identify::Event::Received { peer_id, info, .. }) => {
            Some(Event::PeerIdentified {
                peer: *peer_id,
                listen_addrs: info.listen_addrs.clone(),
            })
        }
        // identify::Event is #[non_exhaustive] — wildcard covers Sent, Pushed, Error
        BehaviourEvent::Identify(_) => None,

        BehaviourEvent::Autonat(autonat::Event::StatusChanged { new, .. }) => {
            let status = match new {
                autonat::NatStatus::Public(_) => NatStatus::Public,
                autonat::NatStatus::Private => NatStatus::Private,
                autonat::NatStatus::Unknown => NatStatus::Unknown,
            };
            Some(Event::NatStatusChanged(status))
        }
        // autonat::Event is #[non_exhaustive] — wildcard covers InboundProbe, OutboundProbe
        BehaviourEvent::Autonat(_) => None,

        BehaviourEvent::RelayClient(relay::client::Event::ReservationReqAccepted {
            relay_peer_id,
            ..
        }) => Some(Event::RelayReservationAccepted {
            relay_peer: *relay_peer_id,
        }),
        BehaviourEvent::RelayClient(event) => {
            tracing::debug!(?event, "relay client event");
            None
        }

        BehaviourEvent::Dcutr(event) => match &event.result {
            Ok(_) => Some(Event::HolePunchSucceeded {
                remote_peer: event.remote_peer_id,
            }),
            Err(e) => Some(Event::HolePunchFailed {
                remote_peer: event.remote_peer_id,
                reason: format!("{e}"),
            }),
        },

        BehaviourEvent::RelayServer(_) => None,
        BehaviourEvent::Ping(ping::Event { peer, result, .. }) => {
            match result {
                Ok(rtt) => tracing::info!(%peer, rtt = ?rtt, "ping"),
                Err(e) => tracing::warn!(%peer, error = %e, "ping failed"),
            }
            None
        }
        BehaviourEvent::Stream(()) => None,
    }
}

// ─── Command execution ──────────────────────────────────────────────────────

fn execute_commands(
    swarm: &mut libp2p::Swarm<Behaviour>,
    commands: &[Command],
    exposed: &[ExposedService],
    active_bootstrap_query: &mut Option<kad::QueryId>,
) {
    for cmd in commands {
        match cmd {
            Command::Dial(addr) => match swarm.dial(addr.clone()) {
                Ok(()) => {}
                Err(e) => tracing::warn!(%addr, error = %e, "dial failed"),
            },
            Command::Listen(addr) => match swarm.listen_on(addr.clone()) {
                Ok(_) => {}
                Err(e) => tracing::warn!(%addr, error = %e, "listen failed"),
            },
            Command::KademliaBootstrap => match swarm.behaviour_mut().kademlia.bootstrap() {
                Ok(query_id) => *active_bootstrap_query = Some(query_id),
                Err(e) => {
                    *active_bootstrap_query = None;
                    tracing::warn!(error = %e, "kademlia bootstrap failed");
                }
            },
            Command::KademliaAddAddress { peer, addr } => {
                swarm
                    .behaviour_mut()
                    .kademlia
                    .add_address(peer, addr.clone());
            }
            Command::KademliaStartProviding { key } => {
                let key = kad::RecordKey::new(key);
                match swarm.behaviour_mut().kademlia.start_providing(key) {
                    Ok(_) => {}
                    Err(e) => tracing::warn!(error = %e, "start_providing failed"),
                }
            }
            Command::KademliaGetProviders { key } => {
                let record_key = kad::RecordKey::new(key);
                swarm.behaviour_mut().kademlia.get_providers(record_key);
            }
            Command::KademliaPutRecord { key, value } => {
                let record = kad::Record::new(key.clone(), value.clone());
                match swarm
                    .behaviour_mut()
                    .kademlia
                    .put_record(record, kad::Quorum::One)
                {
                    Ok(_) => {}
                    Err(e) => tracing::warn!(error = %e, "put_record failed"),
                }
            }
            Command::RequestRelayReservation {
                relay_peer,
                relay_addr,
            } => {
                let mut base = relay_addr
                    .iter()
                    .take_while(|p| !matches!(p, libp2p::multiaddr::Protocol::P2p(_)))
                    .fold(Multiaddr::empty(), |mut acc, p| {
                        acc.push(p);
                        acc
                    });
                base.push(libp2p::multiaddr::Protocol::P2p(*relay_peer));
                base.push(libp2p::multiaddr::Protocol::P2pCircuit);
                tracing::info!(%base, "requesting relay reservation");
                match swarm.listen_on(base) {
                    Ok(_) => {}
                    Err(e) => tracing::warn!(error = %e, "relay reservation listen failed"),
                }
            }
            Command::AddExternalAddress(addr) => {
                swarm.add_external_address(addr.clone());
            }
            Command::PublishServices => {
                publish_services(swarm, exposed);
            }
            Command::Log(msg) => {
                tracing::info!("{msg}");
            }
            Command::Shutdown => {
                // Handled by the event loop checking phase
            }
        }
    }
}

fn publish_services(swarm: &mut libp2p::Swarm<Behaviour>, exposed: &[ExposedService]) {
    for svc in exposed {
        let key = service::service_key(&svc.name);
        let record_key = kad::RecordKey::new(&key);
        match swarm.behaviour_mut().kademlia.start_providing(record_key) {
            Ok(_) => {}
            Err(e) => tracing::warn!(service = %svc.name, error = %e, "start_providing failed"),
        }

        let metadata = match serde_json::to_vec(&svc.name) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(service = %svc.name, error = %e, "serializing service metadata failed");
                continue;
            }
        };
        let record = kad::Record::new(key, metadata);
        match swarm
            .behaviour_mut()
            .kademlia
            .put_record(record, kad::Quorum::One)
        {
            Ok(_) => {}
            Err(e) => tracing::warn!(service = %svc.name, error = %e, "put_record failed"),
        }
    }
}

// ─── Helpers ────────────────────────────────────────────────────────────────

fn is_relayed(endpoint: &libp2p::core::ConnectedPoint) -> bool {
    let addr = match endpoint {
        libp2p::core::ConnectedPoint::Dialer { address, .. } => address,
        libp2p::core::ConnectedPoint::Listener { send_back_addr, .. } => send_back_addr,
    };
    addr.iter()
        .any(|p| matches!(p, libp2p::multiaddr::Protocol::P2pCircuit))
}

fn spawn_tunnels(
    specs: &[(String, ServiceAddr)],
    remote_peer: PeerId,
    stream_control: &libp2p_stream::Control,
    registry: &mut TunnelRegistry,
) {
    for (service_name, bind_addr) in specs {
        let control = stream_control.clone();
        let svc = service_name.clone();
        let addr = bind_addr.clone();
        let label = format!("{remote_peer}:{svc}@{addr}");
        let handle = tokio::spawn(async move {
            match tunnel::connect_tunnel(control, remote_peer, svc, addr).await {
                Ok(()) => {}
                Err(e) => tracing::error!(error = %e, "tunnel failed"),
            }
        });
        registry.register(label, handle);
    }
}

fn initiate_tunnel_lookups(
    swarm: &mut libp2p::Swarm<Behaviour>,
    pending_tunnels: &HashMap<PeerId, Vec<(String, ServiceAddr)>>,
    kad_tunnel_queries: &mut HashMap<kad::QueryId, PeerId>,
) {
    for &target_peer in pending_tunnels.keys() {
        let query_id = swarm
            .behaviour_mut()
            .kademlia
            .get_closest_peers(target_peer);
        kad_tunnel_queries.insert(query_id, target_peer);
        tracing::info!(%target_peer, "starting DHT lookup for tunnel target");
    }
}

fn initiate_service_lookups(
    swarm: &mut libp2p::Swarm<Behaviour>,
    pending_service_tunnels: &HashMap<String, Vec<ServiceAddr>>,
    kad_service_queries: &mut HashMap<kad::QueryId, String>,
) {
    for service_name in pending_service_tunnels.keys() {
        let key = service::service_key(service_name);
        let record_key = kad::RecordKey::new(&key);
        let query_id = swarm.behaviour_mut().kademlia.get_providers(record_key);
        kad_service_queries.insert(query_id, service_name.clone());
        tracing::info!(%service_name, "starting DHT provider lookup");
    }
}

fn extract_peer_id(addr: &Multiaddr) -> Option<PeerId> {
    addr.iter().find_map(|proto| match proto {
        libp2p::multiaddr::Protocol::P2p(peer_id) => Some(peer_id),
        _ => None,
    })
}

fn event_name(event: &Event) -> &'static str {
    match event {
        Event::ListeningOn { .. } => "ListeningOn",
        Event::BootstrapConnected { .. } => "BootstrapConnected",
        Event::ShutdownRequested => "ShutdownRequested",
        Event::KademliaBootstrapOk => "KademliaBootstrapOk",
        Event::KademliaBootstrapFailed { .. } => "KademliaBootstrapFailed",
        Event::MdnsDiscovered { .. } => "MdnsDiscovered",
        Event::MdnsExpired { .. } => "MdnsExpired",
        Event::PeerIdentified { .. } => "PeerIdentified",
        Event::NatStatusChanged(_) => "NatStatusChanged",
        Event::DiscoveryTimeout => "DiscoveryTimeout",
        Event::RelayReservationAccepted { .. } => "RelayReservationAccepted",
        Event::RelayReservationFailed { .. } => "RelayReservationFailed",
        Event::HolePunchSucceeded { .. } => "HolePunchSucceeded",
        Event::HolePunchFailed { .. } => "HolePunchFailed",
        Event::ConnectionLost { .. } => "ConnectionLost",
        Event::ServiceProvidersFound { .. } => "ServiceProvidersFound",
        Event::ServiceLookupFailed { .. } => "ServiceLookupFailed",
        Event::NoBootstrapPeers => "NoBootstrapPeers",
        Event::ExternalAddrConfirmed { .. } => "ExternalAddrConfirmed",
        Event::ExternalAddrExpired { .. } => "ExternalAddrExpired",
    }
}

fn phase_name(phase: Phase) -> &'static str {
    match phase {
        Phase::Initializing => "Initializing",
        Phase::Discovering => "Discovering",
        Phase::Participating => "Participating",
        Phase::ShuttingDown => "ShuttingDown",
    }
}

fn log_phase_transition(event: &str, old: Phase, new: Phase, commands: usize) {
    match (old, new) {
        (Phase::Initializing, Phase::Initializing)
        | (Phase::Discovering, Phase::Discovering)
        | (Phase::Participating, Phase::Participating)
        | (Phase::ShuttingDown, Phase::ShuttingDown) => {}
        (old, new) => {
            tracing::info!(
                event,
                from = phase_name(old),
                to = phase_name(new),
                commands,
                "phase transition"
            );
        }
    }
}

/// Predicate extracted from `translate_behaviour_event`: accepts a Kademlia
/// bootstrap result only when it matches the currently-tracked query and
/// is the final step.
fn is_active_bootstrap_result(
    event_id: kad::QueryId,
    active: Option<kad::QueryId>,
    step_last: bool,
) -> bool {
    active == Some(event_id) && step_last
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    // ─── Generators (shared with state.rs tests) ─────────────────────────

    fn arb_peer_id() -> impl Strategy<Value = PeerId> {
        any::<[u8; 32]>().prop_map(|bytes| {
            // Infallible: any 32 bytes is a valid ed25519 seed
            let secret = libp2p::identity::ed25519::SecretKey::try_from_bytes(bytes)
                .expect("any 32 bytes is a valid ed25519 seed");
            let ed_kp = libp2p::identity::ed25519::Keypair::from(secret);
            let keypair = libp2p::identity::Keypair::from(ed_kp);
            PeerId::from(keypair.public())
        })
    }

    fn arb_multiaddr() -> impl Strategy<Value = Multiaddr> {
        (
            1u8..=254,
            0u8..=255,
            0u8..=255,
            1u8..=254,
            1024u16..65535u16,
        )
            .prop_map(|(a, b, c, d, port)| {
                // Infallible: formatted string is always a valid multiaddr
                format!("/ip4/{a}.{b}.{c}.{d}/tcp/{port}")
                    .parse()
                    .expect("generated IP4/TCP multiaddr is always valid")
            })
    }

    // ─── Suite 1: ReconnectBackoff property tests ────────────────────────

    proptest! {
        #[test]
        fn first_call_returns_initial(
            peer in arb_peer_id(),
            addr in arb_multiaddr(),
        ) {
            let mut backoff = ReconnectBackoff::new(&[(peer, addr.clone())]);
            let result = backoff.schedule_reconnect(peer);
            let (returned_addr, delay) = result.expect("known peer returns Some");
            prop_assert_eq!(returned_addr, addr);
            prop_assert_eq!(delay, BACKOFF_INITIAL);
        }

        #[test]
        fn exponential_growth_sequence(
            peer in arb_peer_id(),
            addr in arb_multiaddr(),
            n in 0usize..10,
        ) {
            let mut backoff = ReconnectBackoff::new(&[(peer, addr)]);
            for k in 0..=n {
                let (_, delay) = backoff.schedule_reconnect(peer)
                    .expect("known peer returns Some");
                // step k returns min(BACKOFF_INITIAL * 2^k, BACKOFF_MAX)
                let expected = BACKOFF_INITIAL.saturating_mul(1u32.checked_shl(u32::try_from(k)
                    .expect("k < 10 fits in u32")).unwrap_or(u32::MAX)).min(BACKOFF_MAX);
                prop_assert_eq!(delay, expected, "mismatch at step {}", k);
            }
        }

        #[test]
        fn delay_never_exceeds_max(
            peer in arb_peer_id(),
            addr in arb_multiaddr(),
            n in 1usize..20,
        ) {
            let mut backoff = ReconnectBackoff::new(&[(peer, addr)]);
            for _ in 0..n {
                let (_, delay) = backoff.schedule_reconnect(peer)
                    .expect("known peer returns Some");
                prop_assert!(delay <= BACKOFF_MAX, "delay {} exceeded max {}", delay.as_secs(), BACKOFF_MAX.as_secs());
            }
        }

        #[test]
        fn cap_is_stable(
            peer in arb_peer_id(),
            addr in arb_multiaddr(),
        ) {
            let mut backoff = ReconnectBackoff::new(&[(peer, addr)]);
            // Advance past the cap (2^6 = 64 > 60)
            for _ in 0..10 {
                backoff.schedule_reconnect(peer);
            }
            // All subsequent calls should return BACKOFF_MAX
            for _ in 0..5 {
                let (_, delay) = backoff.schedule_reconnect(peer)
                    .expect("known peer returns Some");
                prop_assert_eq!(delay, BACKOFF_MAX);
            }
        }

        #[test]
        fn reset_clears_delay(
            peer in arb_peer_id(),
            addr in arb_multiaddr(),
            n in 1usize..10,
        ) {
            let mut backoff = ReconnectBackoff::new(&[(peer, addr)]);
            for _ in 0..n {
                backoff.schedule_reconnect(peer);
            }
            backoff.reset(&peer);
            let (_, delay) = backoff.schedule_reconnect(peer)
                .expect("known peer returns Some after reset");
            prop_assert_eq!(delay, BACKOFF_INITIAL);
        }

        #[test]
        fn unknown_peer_returns_none(
            known_peer in arb_peer_id(),
            unknown_peer in arb_peer_id(),
            addr in arb_multiaddr(),
        ) {
            prop_assume!(known_peer != unknown_peer);
            let mut backoff = ReconnectBackoff::new(&[(known_peer, addr)]);
            let result = backoff.schedule_reconnect(unknown_peer);
            prop_assert!(result.is_none());
        }

        #[test]
        fn independent_peer_delays(
            peer_a in arb_peer_id(),
            peer_b in arb_peer_id(),
            addr_a in arb_multiaddr(),
            addr_b in arb_multiaddr(),
        ) {
            prop_assume!(peer_a != peer_b);
            let mut backoff = ReconnectBackoff::new(&[(peer_a, addr_a), (peer_b, addr_b)]);

            // Advance peer_a 5 times
            for _ in 0..5 {
                backoff.schedule_reconnect(peer_a);
            }

            // peer_b should still return initial
            let (_, delay_b) = backoff.schedule_reconnect(peer_b)
                .expect("peer_b known");
            prop_assert_eq!(delay_b, BACKOFF_INITIAL);
        }
    }

    // ─── Suite 2: Bootstrap QueryId filtering ────────────────────────────

    fn make_query_ids() -> (kad::QueryId, kad::QueryId) {
        let keypair = libp2p::identity::Keypair::generate_ed25519();
        let local_peer = PeerId::from(keypair.public());
        let kad_config = kad::Config::new(crate::protocol::kad_protocol());
        let store = kad::store::MemoryStore::new(local_peer);
        let mut kademlia = kad::Behaviour::with_config(local_peer, store, kad_config);

        // Add a fake peer so bootstrap() has someone to query
        let fake_peer = PeerId::random();
        let fake_addr: Multiaddr = "/ip4/1.2.3.4/tcp/5678"
            .parse()
            .expect("static multiaddr is always valid");
        kademlia.add_address(&fake_peer, fake_addr);

        let qid1 = kademlia
            .bootstrap()
            .expect("bootstrap with known peer succeeds");
        let qid2 = kademlia.get_closest_peers(PeerId::random());
        (qid1, qid2)
    }

    #[test]
    fn matching_id_last_step_accepted() {
        let (qid, _) = make_query_ids();
        assert!(is_active_bootstrap_result(qid, Some(qid), true));
    }

    #[test]
    fn non_matching_id_rejected() {
        let (qid, other) = make_query_ids();
        assert!(!is_active_bootstrap_result(other, Some(qid), true));
    }

    #[test]
    fn intermediate_step_rejected() {
        let (qid, _) = make_query_ids();
        assert!(!is_active_bootstrap_result(qid, Some(qid), false));
    }

    #[test]
    fn no_active_query_rejected() {
        let (qid, _) = make_query_ids();
        assert!(!is_active_bootstrap_result(qid, None, true));
    }

    #[test]
    fn both_none_and_not_last_rejected() {
        let (qid, _) = make_query_ids();
        assert!(!is_active_bootstrap_result(qid, None, false));
    }
}
