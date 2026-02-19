use std::collections::HashMap;

use libp2p::{Multiaddr, PeerId, kad};

use crate::{
    behaviour::Behaviour,
    specs::ExposedService,
    state::Command,
    tunnel::{self, TunnelRegistry},
    types::{KademliaKey, ServiceName},
};

pub struct ExecutionContext {
    pub active_bootstrap_query: Option<kad::QueryId>,
    pub kad_tunnel_queries: HashMap<kad::QueryId, PeerId>,
    pub kad_service_queries: HashMap<kad::QueryId, ServiceName>,
    pub stream_control: libp2p_stream::Control,
    pub tunnel_registry: TunnelRegistry,
}

fn find_local_quic_addr(swarm: &libp2p::Swarm<Behaviour>) -> Option<std::net::SocketAddr> {
    swarm
        .listeners()
        .filter(|addr| {
            !addr
                .iter()
                .any(|p| matches!(p, libp2p::multiaddr::Protocol::P2pCircuit))
        })
        .filter_map(crate::nat_primer::extract_udp_socket_addr)
        .find(|sa| match sa.ip() {
            std::net::IpAddr::V4(v4) => !v4.is_loopback() && !v4.is_unspecified(),
            std::net::IpAddr::V6(v6) => !v6.is_loopback() && !v6.is_unspecified(),
        })
}

pub fn execute_commands(
    swarm: &mut libp2p::Swarm<Behaviour>,
    commands: &[Command],
    exposed: &[ExposedService],
    ctx: &mut ExecutionContext,
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
                Ok(query_id) => ctx.active_bootstrap_query = Some(query_id),
                Err(e) => {
                    ctx.active_bootstrap_query = None;
                    tracing::warn!(error = %e, "kademlia bootstrap failed");
                }
            },
            Command::KademliaAddAddress { peer, addr } => {
                swarm
                    .behaviour_mut()
                    .kademlia
                    .add_address(peer, addr.clone());
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
            Command::Shutdown => {
                // Handled by the event loop checking phase
            }
            Command::DhtLookupPeer { peer } => {
                let query_id = swarm.behaviour_mut().kademlia.get_closest_peers(*peer);
                ctx.kad_tunnel_queries.insert(query_id, *peer);
                tracing::info!(%peer, "starting DHT lookup for tunnel target");
            }
            Command::DhtGetProviders { service_name, key } => {
                let query_id = swarm
                    .behaviour_mut()
                    .kademlia
                    .get_providers(key.clone().into_record_key());
                ctx.kad_service_queries
                    .insert(query_id, service_name.clone());
                tracing::info!(%service_name, "starting DHT provider lookup");
            }
            Command::DialPeer { peer } => match swarm.dial(*peer) {
                Ok(()) => tracing::info!(%peer, "dialing tunnel target"),
                Err(e) => tracing::error!(%peer, error = %e, "failed to dial tunnel target"),
            },
            Command::SpawnTunnel {
                peer,
                service,
                bind,
                relayed,
            } => {
                let control = ctx.stream_control.clone();
                let peer = *peer;
                let svc = service.clone();
                let addr = bind.clone();
                let relayed = *relayed;
                let label = format!("{peer}:{svc}@{addr}");
                tracing::info!(%peer, service = %svc, bind = %addr, relayed, "spawning tunnel");
                let handle = tokio::spawn(async move {
                    match tunnel::connect_tunnel(control, peer, svc, addr).await {
                        Ok(()) => {}
                        Err(e) => tracing::error!(error = %e, "tunnel failed"),
                    }
                });
                ctx.tunnel_registry.register(label, handle);
            }
            Command::AwaitHolePunch { peer } => {
                let external_addrs: Vec<_> = swarm.external_addresses().collect();
                tracing::info!(
                    %peer,
                    addrs = ?external_addrs,
                    "awaiting hole-punch — external address snapshot"
                );
            }
            Command::PrimeNatMapping { peer, peer_addrs } => {
                if let Some(local_addr) = find_local_quic_addr(swarm) {
                    let addrs = peer_addrs.clone();
                    tokio::spawn(async move {
                        crate::nat_primer::send_nat_priming(local_addr, &addrs).await;
                    });
                    tracing::info!(%peer, %local_addr, "NAT priming packets sent");
                } else {
                    tracing::debug!(%peer, "NAT priming skipped — no non-loopback listen address");
                }
            }
            Command::PrimeAndDialDirect { peer, peer_addrs } => {
                if swarm.is_connected(peer) {
                    tracing::debug!(%peer, "skipping primed direct dial — already connected");
                } else {
                    if let Some(local_addr) = find_local_quic_addr(swarm) {
                        let addrs = peer_addrs.clone();
                        tokio::spawn(async move {
                            crate::nat_primer::send_nat_priming(local_addr, &addrs).await;
                        });
                        tracing::info!(%peer, %local_addr, "NAT priming packets sent before direct dial");
                    }

                    for addr in peer_addrs {
                        if !addr
                            .iter()
                            .any(|p| matches!(p, libp2p::multiaddr::Protocol::P2pCircuit))
                        {
                            tracing::info!(%peer, %addr, "primed direct dial attempt");
                            match swarm.dial(addr.clone()) {
                                Ok(()) => break,
                                Err(e) => {
                                    tracing::debug!(%peer, error = %e, "primed direct dial failed")
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

pub fn publish_services(swarm: &mut libp2p::Swarm<Behaviour>, exposed: &[ExposedService]) {
    for svc in exposed {
        let key = KademliaKey::for_service(&svc.name);
        match swarm
            .behaviour_mut()
            .kademlia
            .start_providing(key.clone().into_record_key())
        {
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
        let record = kad::Record::new(key.into_record_key(), metadata);
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
