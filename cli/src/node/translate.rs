use std::collections::{HashMap, HashSet};

#[cfg(feature = "autonat")]
use libp2p::autonat;
#[cfg(feature = "mdns")]
use libp2p::mdns;
use libp2p::{Multiaddr, PeerId, identify, kad, ping, relay, swarm::SwarmEvent};

#[cfg(feature = "autonat")]
use crate::state::NatStatus;
use crate::{behaviour::BehaviourEvent, state::Event, types::ServiceName};

pub fn translate_swarm_event(
    event: &SwarmEvent<BehaviourEvent>,
    bootstrap_peers: &HashSet<PeerId>,
    active_bootstrap_query: Option<kad::QueryId>,
    kad_tunnel_queries: &mut HashMap<kad::QueryId, PeerId>,
    kad_service_queries: &mut HashMap<kad::QueryId, ServiceName>,
    is_connected: impl Fn(&PeerId) -> bool,
) -> Vec<Event> {
    let mut events = Vec::new();

    match event {
        SwarmEvent::NewListenAddr { address, .. } => {
            events.push(Event::ListeningOn {
                addr: address.clone(),
            });
        }

        SwarmEvent::ConnectionEstablished {
            peer_id, endpoint, ..
        } => {
            if bootstrap_peers.contains(peer_id) {
                events.push(Event::BootstrapConnected {
                    peer: *peer_id,
                    addr: endpoint.get_remote_address().clone(),
                });
            }
            events.push(Event::TunnelPeerConnected {
                peer: *peer_id,
                relayed: is_relayed(endpoint),
            });
        }

        SwarmEvent::ConnectionClosed {
            peer_id,
            num_established,
            ..
        } => {
            events.push(Event::ConnectionLost {
                peer: *peer_id,
                remaining_connections: *num_established,
            });
        }

        SwarmEvent::Behaviour(behaviour_event) => {
            events.extend(translate_behaviour_event(
                behaviour_event,
                active_bootstrap_query,
                kad_tunnel_queries,
                kad_service_queries,
                &is_connected,
            ));
        }

        SwarmEvent::ListenerError { error, .. } => {
            tracing::warn!(error = %error, "listener error");
        }
        SwarmEvent::ListenerClosed {
            reason, addresses, ..
        } => {
            tracing::info!(reason = ?reason, "listener closed");
            if reason.is_err()
                && let Some(relay_peer) = extract_relay_peer_from_addrs(addresses)
            {
                let reason_str = match reason {
                    Err(e) => e.to_string(),
                    Ok(()) => "unknown".to_string(),
                };
                events.push(Event::RelayReservationFailed {
                    relay_peer,
                    reason: reason_str,
                });
            }
        }
        SwarmEvent::OutgoingConnectionError { peer_id, error, .. } => {
            let err_msg = error.to_string();
            if err_msg.contains("local peer id") {
                tracing::debug!(peer = ?peer_id, "self-dial attempt (expected with few DHT peers)");
            } else {
                tracing::warn!(peer = ?peer_id, error = %error, "outgoing connection error");
            }
        }

        SwarmEvent::ExternalAddrConfirmed { address } => {
            events.push(Event::ExternalAddrConfirmed {
                addr: address.clone(),
            });
        }

        SwarmEvent::ExternalAddrExpired { address } => {
            events.push(Event::ExternalAddrExpired {
                addr: address.clone(),
            });
        }

        SwarmEvent::IncomingConnection { .. }
        | SwarmEvent::IncomingConnectionError { .. }
        | SwarmEvent::ExpiredListenAddr { .. }
        | SwarmEvent::Dialing { .. }
        | SwarmEvent::NewExternalAddrCandidate { .. }
        | SwarmEvent::NewExternalAddrOfPeer { .. } => {}

        // SwarmEvent is #[non_exhaustive] — wildcard required for forward compatibility
        _ => {}
    }

    events
}

fn translate_behaviour_event(
    event: &BehaviourEvent,
    active_bootstrap_query: Option<kad::QueryId>,
    kad_tunnel_queries: &mut HashMap<kad::QueryId, PeerId>,
    kad_service_queries: &mut HashMap<kad::QueryId, ServiceName>,
    is_connected: &impl Fn(&PeerId) -> bool,
) -> Vec<Event> {
    match event {
        BehaviourEvent::Kademlia(kad::Event::OutboundQueryProgressed {
            id,
            result: kad::QueryResult::Bootstrap(result),
            step,
            ..
        }) => {
            match is_active_bootstrap_result(*id, active_bootstrap_query, step.last) {
                true => {}
                false => return vec![],
            }
            match result {
                Ok(_) => vec![Event::KademliaBootstrapOk],
                Err(e) => vec![Event::KademliaBootstrapFailed {
                    reason: e.to_string(),
                }],
            }
        }

        // GetClosestPeers result — tunnel peer DHT lookup
        BehaviourEvent::Kademlia(kad::Event::OutboundQueryProgressed {
            id,
            result: kad::QueryResult::GetClosestPeers(..),
            step,
            ..
        }) if step.last => match kad_tunnel_queries.remove(id) {
            Some(target_peer) => vec![Event::DhtPeerLookupComplete {
                peer: target_peer,
                connected: is_connected(&target_peer),
            }],
            None => vec![],
        },

        // GetProviders result — service name DHT lookup
        BehaviourEvent::Kademlia(kad::Event::OutboundQueryProgressed {
            id,
            result: kad::QueryResult::GetProviders(result),
            ..
        }) if kad_service_queries.contains_key(id) => {
            translate_get_providers(*id, result, kad_service_queries, is_connected)
        }

        // kad::Event is #[non_exhaustive] — wildcard covers other query types
        BehaviourEvent::Kademlia(_) => vec![],

        #[cfg(feature = "mdns")]
        BehaviourEvent::Mdns(mdns::Event::Discovered(list)) => vec![Event::MdnsDiscovered {
            peers: list.clone(),
        }],
        #[cfg(feature = "mdns")]
        BehaviourEvent::Mdns(mdns::Event::Expired(list)) => vec![Event::MdnsExpired {
            peers: list.clone(),
        }],

        BehaviourEvent::Identify(identify::Event::Received { peer_id, info, .. }) => {
            vec![Event::PeerIdentified {
                peer: *peer_id,
                listen_addrs: info.listen_addrs.clone(),
                observed_addr: info.observed_addr.clone(),
            }]
        }
        // identify::Event is #[non_exhaustive] — wildcard covers Sent, Pushed, Error
        BehaviourEvent::Identify(_) => vec![],

        #[cfg(feature = "autonat")]
        BehaviourEvent::Autonat(autonat::Event::StatusChanged { new, .. }) => {
            let status = match new {
                autonat::NatStatus::Public(_) => NatStatus::Public,
                autonat::NatStatus::Private => NatStatus::Private,
                autonat::NatStatus::Unknown => NatStatus::Unknown,
            };
            vec![Event::NatStatusChanged(status)]
        }
        // autonat::Event is #[non_exhaustive] — wildcard covers InboundProbe, OutboundProbe
        #[cfg(feature = "autonat")]
        BehaviourEvent::Autonat(_) => vec![],

        BehaviourEvent::RelayClient(relay::client::Event::ReservationReqAccepted {
            relay_peer_id,
            ..
        }) => vec![Event::RelayReservationAccepted {
            relay_peer: *relay_peer_id,
        }],
        BehaviourEvent::RelayClient(event) => {
            tracing::debug!(?event, "relay client event");
            vec![]
        }

        BehaviourEvent::Dcutr(event) => match &event.result {
            Ok(_) => vec![Event::HolePunchSucceeded {
                remote_peer: event.remote_peer_id,
            }],
            Err(e) => vec![Event::HolePunchFailed {
                remote_peer: event.remote_peer_id,
                reason: format!("{e}"),
            }],
        },

        BehaviourEvent::RelayServer(_) => vec![],
        BehaviourEvent::Ping(ping::Event { peer, result, .. }) => {
            match result {
                Ok(rtt) => tracing::debug!(%peer, rtt = ?rtt, "ping"),
                Err(e) => tracing::warn!(%peer, error = %e, "ping failed"),
            }
            vec![]
        }
        BehaviourEvent::Stream(()) => vec![],
    }
}

fn translate_get_providers(
    id: kad::QueryId,
    result: &Result<kad::GetProvidersOk, kad::GetProvidersError>,
    kad_service_queries: &mut HashMap<kad::QueryId, ServiceName>,
    is_connected: &impl Fn(&PeerId) -> bool,
) -> Vec<Event> {
    match result {
        Ok(kad::GetProvidersOk::FoundProviders { providers, .. }) => {
            match providers.iter().next() {
                Some(provider) => match kad_service_queries.remove(&id) {
                    Some(service_name) => vec![Event::DhtServiceResolved {
                        service_name,
                        provider: *provider,
                        connected: is_connected(provider),
                    }],
                    None => vec![],
                },
                None => vec![],
            }
        }
        Ok(kad::GetProvidersOk::FinishedWithNoAdditionalRecord { .. }) => {
            match kad_service_queries.remove(&id) {
                Some(service_name) => {
                    tracing::warn!(%service_name, "DHT lookup finished with no providers");
                    vec![Event::DhtServiceFailed {
                        service_name,
                        reason: "no providers found".to_string(),
                    }]
                }
                None => vec![],
            }
        }
        Err(e) => match kad_service_queries.remove(&id) {
            Some(service_name) => {
                tracing::error!(%service_name, error = %e, "service provider lookup failed");
                vec![Event::DhtServiceFailed {
                    service_name,
                    reason: e.to_string(),
                }]
            }
            None => vec![],
        },
    }
}

fn extract_relay_peer_from_addrs(addrs: &[Multiaddr]) -> Option<PeerId> {
    addrs.iter().find_map(|addr| {
        let mut peer = None;
        let mut has_circuit = false;
        for proto in addr.iter() {
            match proto {
                libp2p::multiaddr::Protocol::P2p(pid) => peer = Some(pid),
                libp2p::multiaddr::Protocol::P2pCircuit => has_circuit = true,
                _ => {}
            }
        }
        match has_circuit {
            true => peer,
            false => None,
        }
    })
}

fn is_relayed(endpoint: &libp2p::core::ConnectedPoint) -> bool {
    let addr = match endpoint {
        libp2p::core::ConnectedPoint::Dialer { address, .. } => address,
        libp2p::core::ConnectedPoint::Listener { send_back_addr, .. } => send_back_addr,
    };
    addr.iter()
        .any(|p| matches!(p, libp2p::multiaddr::Protocol::P2pCircuit))
}

pub fn is_active_bootstrap_result(
    event_id: kad::QueryId,
    active: Option<kad::QueryId>,
    step_last: bool,
) -> bool {
    active == Some(event_id) && step_last
}

#[cfg(test)]
mod tests {
    use libp2p::{Multiaddr, PeerId, kad};
    use proptest::prelude::*;

    use super::*;

    fn make_query_ids() -> (kad::QueryId, kad::QueryId) {
        let keypair = libp2p::identity::Keypair::generate_ed25519();
        let local_peer = PeerId::from(keypair.public());
        let kad_config = kad::Config::new(crate::protocol::kad_protocol());
        let store = kad::store::MemoryStore::new(local_peer);
        let mut kademlia = kad::Behaviour::with_config(local_peer, store, kad_config);

        // Infallible: static multiaddr literal is always valid
        let fake_addr: Multiaddr = "/ip4/1.2.3.4/tcp/5678"
            .parse()
            .expect("static multiaddr is always valid");
        kademlia.add_address(&PeerId::random(), fake_addr);

        let qid1 = kademlia
            .bootstrap()
            .expect("bootstrap with known peer succeeds");
        let qid2 = kademlia.get_closest_peers(PeerId::random());
        (qid1, qid2)
    }

    proptest! {
        #[test]
        fn active_bootstrap_accepts_only_matching_last_step(step_last in any::<bool>()) {
            let (qid, other) = make_query_ids();

            // Only matching ID on last step is accepted
            prop_assert_eq!(
                is_active_bootstrap_result(qid, Some(qid), step_last),
                step_last,
            );

            // Non-matching ID always rejected
            prop_assert!(!is_active_bootstrap_result(other, Some(qid), step_last));

            // No active query always rejected
            prop_assert!(!is_active_bootstrap_result(qid, None, step_last));
        }
    }
}
