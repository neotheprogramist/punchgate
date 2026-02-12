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
        // Double for next time, capped at max
        *delay = delay.checked_mul(2).unwrap_or(BACKOFF_MAX);
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

    if config.bootstrap_addrs.is_empty() {
        let old_phase = peer_state.phase;
        let (new_state, commands) = peer_state.transition(Event::NoBootstrapPeers);
        if old_phase != new_state.phase {
            tracing::info!(
                from = phase_name(old_phase),
                to = phase_name(new_state.phase),
                "no bootstrap peers, entering participation directly"
            );
        }
        peer_state = new_state;
        execute_commands(&mut swarm, &commands, &config.exposed);
    }

    let discovery_deadline = tokio::time::sleep(DISCOVERY_TIMEOUT);
    tokio::pin!(discovery_deadline);
    let mut discovery_timeout_fired = false;

    let mut republish_interval = tokio::time::interval(protocol::SERVICE_REPUBLISH_INTERVAL);
    republish_interval.tick().await; // skip first immediate tick

    let mut backoff = ReconnectBackoff::new(&bootstrap_peers_with_addrs);
    let mut pending_reconnects: HashMap<PeerId, tokio::time::Instant> = HashMap::new();

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
                    if swarm.is_connected(&target_peer) {
                        if let Some(specs) = pending_tunnels.remove(&target_peer) {
                            tracing::info!(%target_peer, "tunnel target already connected, spawning tunnels");
                            spawn_tunnels(&specs, target_peer, &stream_control, &mut tunnel_registry);
                        }
                    } else {
                        match swarm.dial(target_peer) {
                            Ok(()) => {
                                tunnel_peers_dialing.insert(target_peer);
                                tracing::info!(%target_peer, "dialing tunnel target after DHT lookup");
                            }
                            Err(e) => {
                                tracing::error!(%target_peer, error = %e, "failed to dial tunnel target");
                                pending_tunnels.remove(&target_peer);
                            }
                        }
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

                                if swarm.is_connected(first_provider) {
                                    tracing::info!(
                                        %service_name, %first_provider,
                                        "service provider already connected, spawning tunnels"
                                    );
                                    spawn_tunnels(&specs, *first_provider, &stream_control, &mut tunnel_registry);
                                } else {
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
                    if is_relayed(endpoint) {
                        tracing::info!(%peer_id, "connected to tunnel target via relay, waiting for DCUtR hole punch");
                    } else if let Some(specs) = pending_tunnels.remove(peer_id) {
                        spawn_tunnels(&specs, *peer_id, &stream_control, &mut tunnel_registry);
                    }
                }

                if let Some(state_event) = translate_swarm_event(
                    &event,
                    &bootstrap_peer_ids,
                ) {
                    if let Event::HolePunchSucceeded { remote_peer } = &state_event {
                        if let Some(specs) = pending_tunnels.remove(remote_peer) {
                            tracing::info!(%remote_peer, "hole punch succeeded, spawning tunnel");
                            spawn_tunnels(&specs, *remote_peer, &stream_control, &mut tunnel_registry);
                        }
                    } else if let Event::HolePunchFailed { remote_peer, reason } = &state_event
                        && pending_tunnels.remove(remote_peer).is_some()
                    {
                        tracing::error!(%remote_peer, %reason, "hole punch failed — tunnel cannot be established (direct connection required)");
                    }

                    let old_phase = peer_state.phase;
                    let event_label = event_name(&state_event);
                    let (new_state, commands) = peer_state.transition(state_event);
                    tracing::debug!(event = event_label, commands = commands.len(), "event processed");
                    if old_phase != new_state.phase {
                        tracing::info!(
                            event = event_label,
                            from = phase_name(old_phase),
                            to = phase_name(new_state.phase),
                            commands = commands.len(),
                            "phase transition"
                        );
                    }
                    peer_state = new_state;
                    execute_commands(
                        &mut swarm,
                        &commands,
                        &config.exposed,
                    );

                    // Initiate DHT lookups when transitioning to Participating
                    if old_phase != Phase::Participating && peer_state.phase == Phase::Participating {
                        initiate_tunnel_lookups(&mut swarm, &pending_tunnels, &mut kad_tunnel_queries);
                        initiate_service_lookups(&mut swarm, &pending_service_tunnels, &mut kad_service_queries);
                    }

                    if peer_state.phase == Phase::ShuttingDown {
                        tracing::info!("shutting down");
                        break;
                    }
                }
            }
            _ = &mut discovery_deadline, if !discovery_timeout_fired => {
                discovery_timeout_fired = true;
                let old_phase = peer_state.phase;
                let (new_state, commands) = peer_state.transition(Event::DiscoveryTimeout);
                tracing::debug!(event = "DiscoveryTimeout", commands = commands.len(), "event processed");
                if old_phase != new_state.phase {
                    tracing::info!(
                        event = "DiscoveryTimeout",
                        from = phase_name(old_phase),
                        to = phase_name(new_state.phase),
                        commands = commands.len(),
                        "phase transition"
                    );
                }
                peer_state = new_state;
                execute_commands(&mut swarm, &commands, &config.exposed);

                if old_phase != Phase::Participating && peer_state.phase == Phase::Participating {
                    initiate_tunnel_lookups(&mut swarm, &pending_tunnels, &mut kad_tunnel_queries);
                    initiate_service_lookups(&mut swarm, &pending_service_tunnels, &mut kad_service_queries);
                }
            }
            _ = republish_interval.tick(), if peer_state.phase == Phase::Participating => {
                publish_services(&mut swarm, &config.exposed);
            }
            _ = async {
                if let Some(deadline) = next_reconnect {
                    tokio::time::sleep_until(deadline).await;
                } else {
                    // No pending reconnects — sleep forever (will be cancelled)
                    std::future::pending::<()>().await;
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
                        if let Err(e) = swarm.dial(addr.clone()) {
                            tracing::warn!(%peer_id, error = %e, "reconnect dial failed");
                            // Schedule another attempt
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
            _ = shutdown::shutdown_signal() => {
                let old_phase = peer_state.phase;
                let (new_state, commands) = peer_state.transition(Event::ShutdownRequested);
                tracing::debug!(event = "ShutdownRequested", commands = commands.len(), "event processed");
                if old_phase != new_state.phase {
                    tracing::info!(
                        event = "ShutdownRequested",
                        from = phase_name(old_phase),
                        to = phase_name(new_state.phase),
                        commands = commands.len(),
                        "phase transition"
                    );
                }
                peer_state = new_state;
                execute_commands(&mut swarm, &commands, &config.exposed);
                if peer_state.phase == Phase::ShuttingDown {
                    tracing::info!("shutting down");
                    break;
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
) -> Option<Event> {
    match event {
        SwarmEvent::NewListenAddr { address, .. } => Some(Event::ListeningOn {
            addr: address.clone(),
        }),

        SwarmEvent::ConnectionEstablished {
            peer_id, endpoint, ..
        } => {
            if bootstrap_peers.contains(peer_id) {
                Some(Event::BootstrapConnected {
                    peer: *peer_id,
                    addr: endpoint.get_remote_address().clone(),
                })
            } else {
                None
            }
        }

        SwarmEvent::ConnectionClosed {
            peer_id,
            num_established,
            ..
        } => Some(Event::ConnectionLost {
            peer: *peer_id,
            remaining_connections: *num_established,
        }),

        SwarmEvent::Behaviour(behaviour_event) => translate_behaviour_event(behaviour_event),

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

fn translate_behaviour_event(event: &BehaviourEvent) -> Option<Event> {
    match event {
        BehaviourEvent::Kademlia(kad::Event::OutboundQueryProgressed {
            result: kad::QueryResult::Bootstrap(result),
            ..
        }) => match result {
            Ok(_) => Some(Event::KademliaBootstrapOk),
            Err(e) => Some(Event::KademliaBootstrapFailed {
                reason: e.to_string(),
            }),
        },
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
) {
    for cmd in commands {
        match cmd {
            Command::Dial(addr) => {
                if let Err(e) = swarm.dial(addr.clone()) {
                    tracing::warn!(%addr, error = %e, "dial failed");
                }
            }
            Command::Listen(addr) => {
                if let Err(e) = swarm.listen_on(addr.clone()) {
                    tracing::warn!(%addr, error = %e, "listen failed");
                }
            }
            Command::KademliaBootstrap => {
                if let Err(e) = swarm.behaviour_mut().kademlia.bootstrap() {
                    tracing::warn!(error = %e, "kademlia bootstrap failed");
                }
            }
            Command::KademliaAddAddress { peer, addr } => {
                swarm
                    .behaviour_mut()
                    .kademlia
                    .add_address(peer, addr.clone());
            }
            Command::KademliaStartProviding { key } => {
                let key = kad::RecordKey::new(key);
                if let Err(e) = swarm.behaviour_mut().kademlia.start_providing(key) {
                    tracing::warn!(error = %e, "start_providing failed");
                }
            }
            Command::KademliaGetProviders { key } => {
                let record_key = kad::RecordKey::new(key);
                swarm.behaviour_mut().kademlia.get_providers(record_key);
            }
            Command::KademliaPutRecord { key, value } => {
                let record = kad::Record::new(key.clone(), value.clone());
                if let Err(e) = swarm
                    .behaviour_mut()
                    .kademlia
                    .put_record(record, kad::Quorum::One)
                {
                    tracing::warn!(error = %e, "put_record failed");
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
                if let Err(e) = swarm.listen_on(base) {
                    tracing::warn!(error = %e, "relay reservation listen failed");
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
        if let Err(e) = swarm.behaviour_mut().kademlia.start_providing(record_key) {
            tracing::warn!(service = %svc.name, error = %e, "start_providing failed");
        }

        let metadata = match serde_json::to_vec(&svc.name) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(service = %svc.name, error = %e, "serializing service metadata failed");
                continue;
            }
        };
        let record = kad::Record::new(key, metadata);
        if let Err(e) = swarm
            .behaviour_mut()
            .kademlia
            .put_record(record, kad::Quorum::One)
        {
            tracing::warn!(service = %svc.name, error = %e, "put_record failed");
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
            if let Err(e) = tunnel::connect_tunnel(control, remote_peer, svc, addr).await {
                tracing::error!(error = %e, "tunnel failed");
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
    addr.iter().find_map(|proto| {
        if let libp2p::multiaddr::Protocol::P2p(peer_id) = proto {
            Some(peer_id)
        } else {
            None
        }
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
