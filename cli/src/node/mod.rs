mod backoff;
mod execute;
mod translate;

use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use anyhow::Result;
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
    protocol::{self, DISCOVERY_TIMEOUT, IDLE_CONNECTION_TIMEOUT},
    shutdown,
    specs::{ExposedService, ServiceAddr, TunnelByNameSpec, TunnelSpec},
    state::{AppState, Event, PeerState, Phase, TunnelState},
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

    let discovery_deadline = tokio::time::sleep(DISCOVERY_TIMEOUT);
    tokio::pin!(discovery_deadline);
    let mut discovery_timeout_fired = false;

    let mut republish_interval = tokio::time::interval(protocol::SERVICE_REPUBLISH_INTERVAL);
    republish_interval.tick().await;

    let mut backoff = ReconnectBackoff::new(&bootstrap_peers_with_addrs);
    let mut pending_reconnects: HashMap<PeerId, tokio::time::Instant> = HashMap::new();

    let shutdown_signal = shutdown::shutdown_signal();
    tokio::pin!(shutdown_signal);

    loop {
        let next_reconnect = pending_reconnects.values().min().copied();

        tokio::select! {
            event = swarm.select_next_some() => {
                if let SwarmEvent::NewListenAddr { address, .. } = &event {
                    tracing::info!("listening on {address}");
                }
                if let SwarmEvent::ConnectionEstablished { peer_id, endpoint, .. } = &event {
                    tracing::info!(peer = %peer_id, endpoint = %endpoint.get_remote_address(), "connection opened");
                }
                if let SwarmEvent::ConnectionClosed { peer_id, num_established, .. } = &event {
                    tracing::info!(peer = %peer_id, remaining = num_established, "connection closed");
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

                if let SwarmEvent::NewExternalAddrCandidate { address } = &event {
                    if external_addr::is_valid_external_candidate(address) {
                        tracing::info!(%address, "new external address candidate (confirming)");
                        swarm.add_external_address(address.clone());
                    } else {
                        tracing::debug!(%address, "ignoring invalid external address candidate");
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

                    match &state_event {
                        Event::RelayReservationAccepted { relay_peer } => {
                            tracing::info!(%relay_peer, "relay reservation accepted");
                        }
                        Event::HolePunchSucceeded { remote_peer } => {
                            tracing::info!(peer = %remote_peer, "hole punch succeeded");
                        }
                        Event::HolePunchFailed { remote_peer, reason } => {
                            let ext_addrs: Vec<_> = swarm.external_addresses().collect();
                            tracing::warn!(peer = %remote_peer, %reason, external_addrs = ?ext_addrs, "hole punch failed");
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

// ─── Helpers ────────────────────────────────────────────────────────────────

fn extract_peer_id(addr: &Multiaddr) -> Option<PeerId> {
    addr.iter().find_map(|proto| match proto {
        libp2p::multiaddr::Protocol::P2p(peer_id) => Some(peer_id),
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
