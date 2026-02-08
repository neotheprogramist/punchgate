use std::{
    collections::{HashMap, HashSet},
    net::SocketAddr,
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result};
use futures::StreamExt;
use libp2p::{
    Multiaddr, PeerId, SwarmBuilder, autonat, identify, kad, mdns, noise, relay, swarm::SwarmEvent,
    yamux,
};

use crate::{
    behaviour::{Behaviour, BehaviourEvent},
    protocol::{self, DISCOVERY_TIMEOUT, IDLE_CONNECTION_TIMEOUT},
    service::{self, ExposedService},
    state::{Command, Event, NatStatus, PeerState, Phase},
    tunnel::{self, TunnelRegistry},
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
        *delay = delay.checked_mul(2).unwrap_or(BACKOFF_MAX).min(BACKOFF_MAX);
        Some((addr.clone(), current))
    }

    fn reset(&mut self, peer: &PeerId) {
        self.delays.remove(peer);
    }
}

// ─── Main entry ─────────────────────────────────────────────────────────────

/// Run the unified peer node.
pub async fn run(
    identity_path: PathBuf,
    listen_addrs: Vec<String>,
    bootstrap_addrs: Vec<String>,
    expose_specs: Vec<String>,
    tunnel_specs: Vec<String>,
) -> Result<()> {
    let keypair = crate::identity::load_or_generate(&identity_path)?;
    let local_peer_id = PeerId::from(keypair.public());
    tracing::info!(%local_peer_id, "starting punchgate node");

    // Parse exposed services
    let exposed: Vec<ExposedService> = expose_specs
        .iter()
        .map(|s| service::parse_expose(s))
        .collect::<Result<_>>()?;

    let services: Arc<std::collections::HashMap<String, SocketAddr>> = Arc::new(
        exposed
            .iter()
            .map(|s| (s.name.clone(), s.local_addr))
            .collect(),
    );

    // Parse bootstrap multiaddrs
    let bootstrap_multiaddrs: Vec<Multiaddr> = bootstrap_addrs
        .iter()
        .map(|s| s.parse().context("invalid bootstrap multiaddr"))
        .collect::<Result<_>>()?;

    // Extract bootstrap peer IDs with their addresses
    let bootstrap_peers_with_addrs: Vec<(PeerId, Multiaddr)> = bootstrap_multiaddrs
        .iter()
        .filter_map(|addr| extract_peer_id(addr).map(|pid| (pid, addr.clone())))
        .collect();

    let bootstrap_peer_ids: HashSet<PeerId> = bootstrap_peers_with_addrs
        .iter()
        .map(|(pid, _)| *pid)
        .collect();

    // Build the swarm
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

    // Get stream control for tunnels
    let mut stream_control = swarm.behaviour().stream.new_control();

    // Start tunnel accept loop
    let incoming = stream_control
        .accept(protocol::tunnel_protocol())
        .map_err(|_| anyhow::anyhow!("tunnel protocol already registered"))?;
    tokio::spawn(tunnel::accept_loop(incoming, services.clone()));

    // Parse and spawn tunnel connections (client side)
    let mut tunnel_registry = TunnelRegistry::new();
    for spec in &tunnel_specs {
        let (remote_peer, service_name, bind_addr) = tunnel::parse_tunnel_spec(spec)?;
        let control = stream_control.clone();
        let label = format!("{remote_peer}:{service_name}@{bind_addr}");
        let handle = tokio::spawn(async move {
            if let Err(e) =
                tunnel::connect_tunnel(control, remote_peer, service_name, bind_addr).await
            {
                tracing::warn!(error = %e, "tunnel connect failed");
            }
        });
        tunnel_registry.register(label, handle);
    }

    // Listen on configured addresses
    for addr_str in &listen_addrs {
        let addr: Multiaddr = addr_str.parse().context("invalid listen multiaddr")?;
        swarm.listen_on(addr)?;
    }

    // Dial bootstrap peers
    for addr in &bootstrap_multiaddrs {
        tracing::info!(%addr, "dialing bootstrap peer");
        swarm.dial(addr.clone())?;
    }

    // Initialize state machine
    let mut peer_state = PeerState::new();
    for peer_id in &bootstrap_peer_ids {
        peer_state.bootstrap_peers.insert(*peer_id);
    }

    // Discovery timeout timer
    let discovery_deadline = tokio::time::sleep(DISCOVERY_TIMEOUT);
    tokio::pin!(discovery_deadline);
    let mut discovery_timeout_fired = false;

    // Service republish timer
    let mut republish_interval = tokio::time::interval(protocol::SERVICE_REPUBLISH_INTERVAL);
    republish_interval.tick().await; // skip first immediate tick

    // Reconnection backoff for bootstrap peers
    let mut backoff = ReconnectBackoff::new(&bootstrap_peers_with_addrs);

    // Pending reconnections (peer → when to reconnect)
    let mut pending_reconnects: HashMap<PeerId, tokio::time::Instant> = HashMap::new();

    loop {
        // Find the earliest reconnect deadline
        let next_reconnect = pending_reconnects.values().min().copied();

        tokio::select! {
            event = swarm.select_next_some() => {
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

                if let Some(state_event) = translate_swarm_event(
                    &event,
                    &bootstrap_peer_ids,
                ) {
                    let (new_state, commands) = peer_state.transition(state_event);
                    peer_state = new_state;
                    execute_commands(
                        &mut swarm,
                        &commands,
                        &exposed,
                    );
                    if peer_state.phase == Phase::ShuttingDown {
                        tracing::info!("shutting down");
                        break;
                    }
                }
            }
            _ = &mut discovery_deadline, if !discovery_timeout_fired => {
                discovery_timeout_fired = true;
                let (new_state, commands) = peer_state.transition(Event::DiscoveryTimeout);
                peer_state = new_state;
                execute_commands(&mut swarm, &commands, &exposed);
            }
            _ = republish_interval.tick(), if peer_state.phase == Phase::Participating => {
                publish_services(&mut swarm, &exposed);
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
            _ = tokio::signal::ctrl_c() => {
                let (new_state, commands) = peer_state.transition(Event::ShutdownRequested);
                peer_state = new_state;
                execute_commands(&mut swarm, &commands, &exposed);
                if peer_state.phase == Phase::ShuttingDown {
                    tracing::info!("shutting down");
                    break;
                }
            }
        }
    }

    // Graceful tunnel shutdown
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

        SwarmEvent::IncomingConnection { .. }
        | SwarmEvent::IncomingConnectionError { .. }
        | SwarmEvent::OutgoingConnectionError { .. }
        | SwarmEvent::ExpiredListenAddr { .. }
        | SwarmEvent::ListenerClosed { .. }
        | SwarmEvent::ListenerError { .. }
        | SwarmEvent::Dialing { .. }
        | SwarmEvent::NewExternalAddrCandidate { .. }
        | SwarmEvent::ExternalAddrConfirmed { .. }
        | SwarmEvent::ExternalAddrExpired { .. }
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

        BehaviourEvent::Mdns(mdns::Event::Discovered(list)) => Some(Event::MdnsDiscovered {
            peers: list.clone(),
        }),
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
        // relay::client::Event is #[non_exhaustive]
        BehaviourEvent::RelayClient(_) => None,

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
        BehaviourEvent::Ping(_) => None,
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
            Command::RequestRelayReservation { relay_addr, .. } => {
                let relay_circuit_addr = relay_addr
                    .clone()
                    .with(libp2p::multiaddr::Protocol::P2pCircuit);
                if let Err(e) = swarm.listen_on(relay_circuit_addr) {
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

fn extract_peer_id(addr: &Multiaddr) -> Option<PeerId> {
    addr.iter().find_map(|proto| {
        if let libp2p::multiaddr::Protocol::P2p(peer_id) = proto {
            Some(peer_id)
        } else {
            None
        }
    })
}
