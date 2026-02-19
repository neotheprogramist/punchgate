use std::collections::{HashMap, HashSet};

use libp2p::{Multiaddr, PeerId};

use super::{command::Command, event::Event, peer::Phase};
use crate::{
    nat_probe::NatMapping,
    specs::ServiceAddr,
    traits::MealyMachine,
    types::{KademliaKey, ServiceName},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TunnelState {
    pending_by_peer: HashMap<PeerId, Vec<(ServiceName, ServiceAddr)>>,
    pending_by_service: HashMap<ServiceName, Vec<ServiceAddr>>,
    dialing: HashSet<PeerId>,
    awaiting_holepunch: HashMap<PeerId, Vec<(ServiceName, ServiceAddr)>>,
    relayed_peers: HashSet<PeerId>,
    holepunch_failed: HashSet<PeerId>,
    nat_mapping: NatMapping,
    peer_external_addrs: HashMap<PeerId, Vec<Multiaddr>>,
}

impl Default for TunnelState {
    fn default() -> Self {
        Self::new()
    }
}

impl TunnelState {
    pub fn new() -> Self {
        Self {
            pending_by_peer: HashMap::new(),
            pending_by_service: HashMap::new(),
            dialing: HashSet::new(),
            awaiting_holepunch: HashMap::new(),
            relayed_peers: HashSet::new(),
            holepunch_failed: HashSet::new(),
            nat_mapping: NatMapping::Unknown,
            peer_external_addrs: HashMap::new(),
        }
    }

    pub fn set_nat_mapping(&mut self, mapping: NatMapping) {
        self.nat_mapping = mapping;
    }

    pub fn add_peer_tunnel(&mut self, peer: PeerId, service: ServiceName, bind: ServiceAddr) {
        self.pending_by_peer
            .entry(peer)
            .or_default()
            .push((service, bind));
    }

    pub fn add_service_tunnel(&mut self, service: ServiceName, bind: ServiceAddr) {
        self.pending_by_service
            .entry(service)
            .or_default()
            .push(bind);
    }

    pub fn has_pending_for(&self, peer: &PeerId) -> bool {
        self.pending_by_peer.contains_key(peer)
    }

    fn initiate_lookups(&self) -> Vec<Command> {
        self.pending_by_peer
            .keys()
            .map(|&peer| Command::DhtLookupPeer { peer })
            .chain(
                self.pending_by_service
                    .keys()
                    .map(|service_name| Command::DhtGetProviders {
                        service_name: service_name.clone(),
                        key: KademliaKey::for_service(service_name),
                    }),
            )
            .collect()
    }

    fn route_connected_peer(
        &mut self,
        peer: PeerId,
        specs: Vec<(ServiceName, ServiceAddr)>,
    ) -> Vec<Command> {
        if self.holepunch_failed.contains(&peer) || !self.nat_mapping.is_holepunch_viable() {
            specs
                .into_iter()
                .map(|(service, bind)| Command::SpawnTunnel {
                    peer,
                    service,
                    bind,
                    relayed: true,
                })
                .collect()
        } else if self.relayed_peers.contains(&peer) {
            self.awaiting_holepunch
                .entry(peer)
                .or_default()
                .extend(specs);
            let peer_addrs = self
                .peer_external_addrs
                .get(&peer)
                .cloned()
                .unwrap_or_default();
            vec![
                Command::PrimeNatMapping { peer, peer_addrs },
                Command::AwaitHolePunch { peer },
            ]
        } else {
            specs
                .into_iter()
                .map(|(service, bind)| Command::SpawnTunnel {
                    peer,
                    service,
                    bind,
                    relayed: false,
                })
                .collect()
        }
    }
}

impl MealyMachine for TunnelState {
    type Event = Event;
    type Command = Command;

    fn transition(mut self, event: Event) -> (Self, Vec<Command>) {
        let mut commands = Vec::new();

        match event {
            Event::PhaseChanged {
                new: Phase::Ready, ..
            } => {
                commands.push(Command::PublishServices);
                commands.extend(self.initiate_lookups());
            }

            Event::RelayReservationAccepted { .. } => {
                commands.push(Command::PublishServices);
            }

            Event::DhtPeerLookupComplete { peer, connected } => match connected {
                true => {
                    if let Some(specs) = self.pending_by_peer.remove(&peer) {
                        commands.extend(self.route_connected_peer(peer, specs));
                    }
                }
                false => {
                    if self.pending_by_peer.contains_key(&peer) {
                        self.dialing.insert(peer);
                        commands.push(Command::DialPeer { peer });
                    }
                }
            },

            Event::DhtServiceResolved {
                service_name,
                provider,
                connected,
            } => {
                if let Some(bind_addrs) = self.pending_by_service.remove(&service_name) {
                    let specs: Vec<(ServiceName, ServiceAddr)> = bind_addrs
                        .into_iter()
                        .map(|addr| (service_name.clone(), addr))
                        .collect();

                    match connected {
                        true => {
                            commands.extend(self.route_connected_peer(provider, specs));
                        }
                        false => {
                            self.pending_by_peer
                                .entry(provider)
                                .or_default()
                                .extend(specs);
                            commands.push(Command::DhtLookupPeer { peer: provider });
                        }
                    }
                }
            }

            Event::DhtServiceFailed { service_name, .. } => {
                self.pending_by_service.remove(&service_name);
            }

            Event::TunnelPeerConnected { peer, relayed } => {
                self.dialing.remove(&peer);
                match relayed {
                    true => {
                        self.relayed_peers.insert(peer);
                        let holepunch_viable = self.nat_mapping.is_holepunch_viable();

                        if let Some(specs) = self.pending_by_peer.remove(&peer) {
                            if holepunch_viable {
                                self.awaiting_holepunch
                                    .entry(peer)
                                    .or_default()
                                    .extend(specs);
                                let peer_addrs = self
                                    .peer_external_addrs
                                    .get(&peer)
                                    .cloned()
                                    .unwrap_or_default();
                                commands.push(Command::PrimeNatMapping { peer, peer_addrs });
                                commands.push(Command::AwaitHolePunch { peer });
                            } else {
                                commands.extend(specs.into_iter().map(|(service, bind)| {
                                    Command::SpawnTunnel {
                                        peer,
                                        service,
                                        bind,
                                        relayed: true,
                                    }
                                }));
                            }
                        } else if holepunch_viable {
                            let peer_addrs = self
                                .peer_external_addrs
                                .get(&peer)
                                .cloned()
                                .unwrap_or_default();
                            commands.push(Command::PrimeNatMapping { peer, peer_addrs });
                        }
                    }
                    false => {
                        self.relayed_peers.remove(&peer);
                        self.holepunch_failed.remove(&peer);
                        commands.extend(
                            self.pending_by_peer
                                .remove(&peer)
                                .into_iter()
                                .chain(self.awaiting_holepunch.remove(&peer))
                                .flatten()
                                .map(|(service, bind)| Command::SpawnTunnel {
                                    peer,
                                    service,
                                    bind,
                                    relayed: false,
                                }),
                        );
                    }
                }
            }

            Event::HolePunchSucceeded { remote_peer } => {
                self.relayed_peers.remove(&remote_peer);
                self.holepunch_failed.remove(&remote_peer);
                commands.extend(
                    self.awaiting_holepunch
                        .remove(&remote_peer)
                        .into_iter()
                        .flatten()
                        .map(|(service, bind)| Command::SpawnTunnel {
                            peer: remote_peer,
                            service,
                            bind,
                            relayed: false,
                        }),
                );
            }

            Event::HolePunchFailed {
                remote_peer,
                ref reason,
            } => {
                self.holepunch_failed.insert(remote_peer);
                if let Some(specs) = self.awaiting_holepunch.remove(&remote_peer) {
                    tracing::info!(
                        peer = %remote_peer,
                        reason = %reason,
                        "hole punch failed, falling back to relay for tunnel"
                    );
                    commands.extend(specs.into_iter().map(|(service, bind)| {
                        Command::SpawnTunnel {
                            peer: remote_peer,
                            service,
                            bind,
                            relayed: true,
                        }
                    }));
                }
            }

            Event::HolePunchTimeout { peer } => {
                self.holepunch_failed.insert(peer);
                if let Some(specs) = self.awaiting_holepunch.remove(&peer) {
                    tracing::info!(
                        peer = %peer,
                        "hole punch timeout, falling back to relay for tunnel"
                    );
                    commands.extend(specs.into_iter().map(|(service, bind)| {
                        Command::SpawnTunnel {
                            peer,
                            service,
                            bind,
                            relayed: true,
                        }
                    }));
                }
            }

            Event::ConnectionLost {
                peer,
                remaining_connections: 0,
            } => {
                self.relayed_peers.remove(&peer);
                self.holepunch_failed.remove(&peer);
                self.peer_external_addrs.remove(&peer);
                if let Some(specs) = self.awaiting_holepunch.remove(&peer) {
                    for (service, _bind) in &specs {
                        tracing::error!(
                            peer = %peer,
                            service = %service,
                            "tunnel request dropped: all connections to peer lost"
                        );
                    }
                }
            }

            Event::HolePunchRetryTick { peer } => {
                if self.relayed_peers.contains(&peer) {
                    let peer_addrs = self
                        .peer_external_addrs
                        .get(&peer)
                        .cloned()
                        .unwrap_or_default();
                    commands.push(Command::PrimeAndDialDirect { peer, peer_addrs });
                }
            }

            Event::NatMappingDetected(mapping) => {
                self.nat_mapping = mapping;
            }

            Event::PeerIdentified {
                peer, listen_addrs, ..
            } => {
                if !listen_addrs.is_empty() {
                    self.peer_external_addrs.insert(peer, listen_addrs.clone());
                }
            }

            Event::PhaseChanged { .. }
            | Event::ListeningOn { .. }
            | Event::BootstrapConnected { .. }
            | Event::ShutdownRequested
            | Event::KademliaBootstrapOk
            | Event::KademliaBootstrapFailed { .. }
            | Event::MdnsDiscovered { .. }
            | Event::MdnsExpired { .. }
            | Event::NatStatusChanged(_)
            | Event::DiscoveryTimeout
            | Event::RelayReservationFailed { .. }
            | Event::ConnectionLost { .. }
            | Event::NoBootstrapPeers
            | Event::ExternalAddrConfirmed { .. }
            | Event::ExternalAddrExpired { .. } => {}
        }

        (self, commands)
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::test_utils::{
        arb_multiaddr, arb_nat_status, arb_peer_id, arb_phase, arb_service_addr, arb_service_name,
    };

    proptest! {
        #[test]
        fn phase_changed_to_ready_publishes(
            old in arb_phase(),
        ) {
            prop_assume!(old != Phase::Ready);
            let state = TunnelState::new();
            let (_, commands) = state.transition(Event::PhaseChanged {
                old,
                new: Phase::Ready,
            });
            prop_assert!(commands.contains(&Command::PublishServices));
        }

        #[test]
        fn phase_changed_to_ready_initiates_lookups(
            old in arb_phase(),
            peer in arb_peer_id(),
            service in arb_service_name(),
            bind in arb_service_addr(),
            svc_name in arb_service_name(),
            svc_bind in arb_service_addr(),
        ) {
            prop_assume!(old != Phase::Ready);
            let mut state = TunnelState::new();
            state.add_peer_tunnel(peer, service, bind);
            state.add_service_tunnel(svc_name.clone(), svc_bind);

            let (_, commands) = state.transition(Event::PhaseChanged {
                old,
                new: Phase::Ready,
            });

            prop_assert!(commands.contains(&Command::PublishServices));
            let has_peer_lookup = commands.iter().any(|c| matches!(
                c, Command::DhtLookupPeer { peer: p } if *p == peer
            ));
            prop_assert!(has_peer_lookup);
            let has_svc_lookup = commands.iter().any(|c| matches!(
                c, Command::DhtGetProviders { .. }
            ));
            prop_assert!(has_svc_lookup);
        }

        #[test]
        fn phase_changed_non_ready_is_noop(
            old in arb_phase(),
            new in arb_phase(),
        ) {
            prop_assume!(new != Phase::Ready);
            let state = TunnelState::new();
            let (_, commands) = state.transition(Event::PhaseChanged { old, new });
            prop_assert!(commands.is_empty());
        }

        #[test]
        fn relay_accepted_publishes(relay_peer in arb_peer_id()) {
            let state = TunnelState::new();
            let (_, commands) = state.transition(Event::RelayReservationAccepted { relay_peer });
            prop_assert!(commands.contains(&Command::PublishServices));
        }

        #[test]
        fn dht_peer_lookup_spawns_when_connected(
            peer in arb_peer_id(),
            service in arb_service_name(),
            bind in arb_service_addr(),
        ) {
            let mut state = TunnelState::new();
            state.add_peer_tunnel(peer, service.clone(), bind.clone());

            let (state, commands) = state.transition(Event::DhtPeerLookupComplete {
                peer, connected: true,
            });

            let has_spawn = commands.contains(&Command::SpawnTunnel {
                peer, service, bind, relayed: false,
            });
            prop_assert!(has_spawn);
            prop_assert!(!state.has_pending_for(&peer));
        }

        #[test]
        fn dht_peer_lookup_dials_when_disconnected(
            peer in arb_peer_id(),
            service in arb_service_name(),
            bind in arb_service_addr(),
        ) {
            let mut state = TunnelState::new();
            state.add_peer_tunnel(peer, service, bind);

            let (state, commands) = state.transition(Event::DhtPeerLookupComplete {
                peer, connected: false,
            });

            let has_dial = commands.contains(&Command::DialPeer { peer });
            prop_assert!(has_dial);
            prop_assert!(state.dialing.contains(&peer));
            prop_assert!(state.has_pending_for(&peer));
        }

        #[test]
        fn dht_service_resolved_spawns_when_connected(
            service in arb_service_name(),
            bind in arb_service_addr(),
            provider in arb_peer_id(),
        ) {
            let mut state = TunnelState::new();
            state.add_service_tunnel(service.clone(), bind.clone());

            let (_, commands) = state.transition(Event::DhtServiceResolved {
                service_name: service.clone(),
                provider,
                connected: true,
            });

            let has_spawn = commands.contains(&Command::SpawnTunnel {
                peer: provider, service, bind, relayed: false,
            });
            prop_assert!(has_spawn);
        }

        #[test]
        fn dht_service_resolved_initiates_peer_lookup_when_disconnected(
            service in arb_service_name(),
            bind in arb_service_addr(),
            provider in arb_peer_id(),
        ) {
            let mut state = TunnelState::new();
            state.add_service_tunnel(service.clone(), bind);

            let (state, commands) = state.transition(Event::DhtServiceResolved {
                service_name: service,
                provider,
                connected: false,
            });

            let has_lookup = commands.contains(&Command::DhtLookupPeer { peer: provider });
            prop_assert!(has_lookup);
            prop_assert!(state.has_pending_for(&provider));
        }

        #[test]
        fn tunnel_peer_connected_direct_spawns_immediately(
            peer in arb_peer_id(),
            service in arb_service_name(),
            bind in arb_service_addr(),
        ) {
            let mut state = TunnelState::new();
            state.add_peer_tunnel(peer, service.clone(), bind.clone());

            let (_, commands) = state.transition(Event::TunnelPeerConnected {
                peer, relayed: false,
            });

            let has_spawn = commands.contains(&Command::SpawnTunnel {
                peer, service, bind, relayed: false,
            });
            prop_assert!(has_spawn);
        }

        #[test]
        fn tunnel_peer_connected_relayed_waits_for_holepunch(
            peer in arb_peer_id(),
            service in arb_service_name(),
            bind in arb_service_addr(),
        ) {
            let mut state = TunnelState::new();
            state.add_peer_tunnel(peer, service, bind);

            let (state, commands) = state.transition(Event::TunnelPeerConnected {
                peer, relayed: true,
            });

            let has_await = commands.contains(&Command::AwaitHolePunch { peer });
            prop_assert!(has_await);
            prop_assert!(state.awaiting_holepunch.contains_key(&peer));
        }

        #[test]
        fn hole_punch_succeeded_spawns_waiting_tunnels(
            peer in arb_peer_id(),
            service in arb_service_name(),
            bind in arb_service_addr(),
        ) {
            let mut state = TunnelState::new();
            state.awaiting_holepunch.insert(peer, vec![(service.clone(), bind.clone())]);

            let (state, commands) = state.transition(Event::HolePunchSucceeded {
                remote_peer: peer,
            });

            let has_spawn = commands.contains(&Command::SpawnTunnel {
                peer, service, bind, relayed: false,
            });
            prop_assert!(has_spawn);
            prop_assert!(!state.awaiting_holepunch.contains_key(&peer));
        }

        #[test]
        fn hole_punch_failed_spawns_on_relay(
            peer in arb_peer_id(),
            service in arb_service_name(),
            bind in arb_service_addr(),
            reason in "[a-z ]{1,20}",
        ) {
            let mut state = TunnelState::new();
            state.awaiting_holepunch.insert(peer, vec![(service.clone(), bind.clone())]);

            let (state, commands) = state.transition(Event::HolePunchFailed {
                remote_peer: peer, reason,
            });

            let has_spawn = commands.contains(&Command::SpawnTunnel {
                peer, service, bind, relayed: true,
            });
            prop_assert!(has_spawn);
            prop_assert!(!state.awaiting_holepunch.contains_key(&peer));
        }

        #[test]
        fn peer_lifecycle_events_are_noop(
            peer in arb_peer_id(),
            addr in arb_multiaddr(),
            nat_status in arb_nat_status(),
        ) {
            let state = TunnelState::new();

            let (_, c) = state.clone().transition(Event::ListeningOn { addr: addr.clone() });
            prop_assert!(c.is_empty());

            let (_, c) = state.clone().transition(Event::BootstrapConnected {
                peer, addr,
            });
            prop_assert!(c.is_empty());

            let (_, c) = state.clone().transition(Event::NatStatusChanged(nat_status));
            prop_assert!(c.is_empty());

            let (_, c) = state.clone().transition(Event::KademliaBootstrapOk);
            prop_assert!(c.is_empty());

            let (_, c) = state.clone().transition(Event::DiscoveryTimeout);
            prop_assert!(c.is_empty());

            let (_, c) = state.clone().transition(Event::ShutdownRequested);
            prop_assert!(c.is_empty());
        }

        // ─── Relay-aware tunnel tests ────────────────────────────────────

        #[test]
        fn dht_peer_lookup_relayed_awaits_holepunch(
            peer in arb_peer_id(),
            service in arb_service_name(),
            bind in arb_service_addr(),
        ) {
            let mut state = TunnelState::new();
            state.relayed_peers.insert(peer);
            state.add_peer_tunnel(peer, service.clone(), bind.clone());

            let (state, commands) = state.transition(Event::DhtPeerLookupComplete {
                peer, connected: true,
            });

            let has_await = commands.contains(&Command::AwaitHolePunch { peer });
            prop_assert!(has_await);
            prop_assert!(!state.pending_by_peer.contains_key(&peer));
            prop_assert!(state.awaiting_holepunch.contains_key(&peer));
            let waiting = state.awaiting_holepunch
                .get(&peer)
                .expect("peer should be in awaiting_holepunch after relayed lookup");
            prop_assert!(waiting.contains(&(service, bind)));
        }

        #[test]
        fn dht_service_resolved_relayed_awaits_holepunch(
            service in arb_service_name(),
            bind in arb_service_addr(),
            provider in arb_peer_id(),
        ) {
            let mut state = TunnelState::new();
            state.relayed_peers.insert(provider);
            state.add_service_tunnel(service.clone(), bind.clone());

            let (state, commands) = state.transition(Event::DhtServiceResolved {
                service_name: service.clone(),
                provider,
                connected: true,
            });

            let has_await = commands.contains(&Command::AwaitHolePunch { peer: provider });
            prop_assert!(has_await);
            prop_assert!(state.awaiting_holepunch.contains_key(&provider));
            let waiting = state.awaiting_holepunch
                .get(&provider)
                .expect("provider should be in awaiting_holepunch after relayed resolve");
            prop_assert!(waiting.contains(&(service, bind)));
        }

        #[test]
        fn dht_peer_lookup_holepunch_failed_spawns_on_relay(
            peer in arb_peer_id(),
            service in arb_service_name(),
            bind in arb_service_addr(),
        ) {
            let mut state = TunnelState::new();
            state.holepunch_failed.insert(peer);
            state.add_peer_tunnel(peer, service.clone(), bind.clone());

            let (state, commands) = state.transition(Event::DhtPeerLookupComplete {
                peer, connected: true,
            });

            let has_spawn = commands.contains(&Command::SpawnTunnel {
                peer, service, bind, relayed: true,
            });
            prop_assert!(has_spawn);
            prop_assert!(!state.pending_by_peer.contains_key(&peer));
        }

        #[test]
        fn dht_service_resolved_holepunch_failed_spawns_on_relay(
            service in arb_service_name(),
            bind in arb_service_addr(),
            provider in arb_peer_id(),
        ) {
            let mut state = TunnelState::new();
            state.holepunch_failed.insert(provider);
            state.add_service_tunnel(service.clone(), bind.clone());

            let (_state, commands) = state.transition(Event::DhtServiceResolved {
                service_name: service.clone(),
                provider,
                connected: true,
            });

            let has_spawn = commands.contains(&Command::SpawnTunnel {
                peer: provider, service, bind, relayed: true,
            });
            prop_assert!(has_spawn);
        }

        #[test]
        fn holepunch_failed_records_failure(
            peer in arb_peer_id(),
            service in arb_service_name(),
            bind in arb_service_addr(),
            reason in "[a-z ]{1,20}",
        ) {
            let mut state = TunnelState::new();
            state.awaiting_holepunch.insert(peer, vec![(service, bind)]);

            let (state, _) = state.transition(Event::HolePunchFailed {
                remote_peer: peer, reason,
            });

            prop_assert!(state.holepunch_failed.contains(&peer));
            prop_assert!(!state.awaiting_holepunch.contains_key(&peer));
        }

        #[test]
        fn direct_connection_spawns_from_awaiting_holepunch(
            peer in arb_peer_id(),
            service in arb_service_name(),
            bind in arb_service_addr(),
        ) {
            let mut state = TunnelState::new();
            state.relayed_peers.insert(peer);
            state.awaiting_holepunch.insert(peer, vec![(service.clone(), bind.clone())]);

            let (state, commands) = state.transition(Event::TunnelPeerConnected {
                peer, relayed: false,
            });

            let has_spawn = commands.contains(&Command::SpawnTunnel {
                peer, service, bind, relayed: false,
            });
            prop_assert!(has_spawn);
            prop_assert!(!state.awaiting_holepunch.contains_key(&peer));
            prop_assert!(!state.relayed_peers.contains(&peer));
        }

        #[test]
        fn connection_lost_cleans_tracking_state(
            peer in arb_peer_id(),
            service in arb_service_name(),
            bind in arb_service_addr(),
        ) {
            let mut state = TunnelState::new();
            state.relayed_peers.insert(peer);
            state.holepunch_failed.insert(peer);
            state.awaiting_holepunch.insert(peer, vec![(service, bind)]);
            state.peer_external_addrs.insert(peer, vec![]);

            let (state, commands) = state.transition(Event::ConnectionLost {
                peer, remaining_connections: 0,
            });

            prop_assert!(commands.is_empty());
            prop_assert!(!state.relayed_peers.contains(&peer));
            prop_assert!(!state.holepunch_failed.contains(&peer));
            prop_assert!(!state.awaiting_holepunch.contains_key(&peer));
            prop_assert!(!state.peer_external_addrs.contains_key(&peer));
        }

        #[test]
        fn holepunch_failed_then_direct_clears_failure(
            peer in arb_peer_id(),
            service in arb_service_name(),
            bind in arb_service_addr(),
            reason in "[a-z ]{1,20}",
        ) {
            let mut state = TunnelState::new();
            state.relayed_peers.insert(peer);
            state.awaiting_holepunch.insert(peer, vec![(service.clone(), bind.clone())]);

            // First: hole punch fails
            let (mut state, _) = state.transition(Event::HolePunchFailed {
                remote_peer: peer, reason,
            });
            prop_assert!(state.holepunch_failed.contains(&peer));

            // Then: add a new tunnel request and get a direct connection
            state.add_peer_tunnel(peer, service.clone(), bind.clone());
            let (state, commands) = state.transition(Event::TunnelPeerConnected {
                peer, relayed: false,
            });

            prop_assert!(!state.holepunch_failed.contains(&peer));
            prop_assert!(!state.relayed_peers.contains(&peer));
            let has_spawn = commands.contains(&Command::SpawnTunnel {
                peer, service, bind, relayed: false,
            });
            prop_assert!(has_spawn);
        }

        #[test]
        fn holepunch_succeeded_spawns_multiple_waiting_tunnels(
            peer in arb_peer_id(),
            s1 in arb_service_name(),
            b1 in arb_service_addr(),
            s2 in arb_service_name(),
            b2 in arb_service_addr(),
        ) {
            let mut state = TunnelState::new();
            state.awaiting_holepunch.insert(peer, vec![
                (s1.clone(), b1.clone()),
                (s2.clone(), b2.clone()),
            ]);

            let (state, commands) = state.transition(Event::HolePunchSucceeded {
                remote_peer: peer,
            });

            prop_assert_eq!(commands.len(), 2);
            let has_first = commands.contains(&Command::SpawnTunnel {
                peer, service: s1, bind: b1, relayed: false,
            });
            prop_assert!(has_first);
            let has_second = commands.contains(&Command::SpawnTunnel {
                peer, service: s2, bind: b2, relayed: false,
            });
            prop_assert!(has_second);
            prop_assert!(!state.awaiting_holepunch.contains_key(&peer));
        }

        #[test]
        fn holepunch_timeout_spawns_on_relay(
            peer in arb_peer_id(),
            service in arb_service_name(),
            bind in arb_service_addr(),
        ) {
            let mut state = TunnelState::new();
            state.relayed_peers.insert(peer);
            state.awaiting_holepunch.insert(peer, vec![(service.clone(), bind.clone())]);

            let (state, commands) = state.transition(Event::HolePunchTimeout { peer });

            let has_spawn = commands.contains(&Command::SpawnTunnel {
                peer, service, bind, relayed: true,
            });
            prop_assert!(has_spawn);
            prop_assert!(!state.awaiting_holepunch.contains_key(&peer));
            prop_assert!(state.holepunch_failed.contains(&peer));
        }

        #[test]
        fn holepunch_retry_dials_relayed_peer(
            peer in arb_peer_id(),
        ) {
            let mut state = TunnelState::new();
            state.relayed_peers.insert(peer);

            let (_, commands) = state.transition(Event::HolePunchRetryTick { peer });

            let has_dial = commands.iter().any(|c| matches!(c, Command::PrimeAndDialDirect { peer: p, .. } if *p == peer));
            prop_assert!(has_dial);
        }

        #[test]
        fn holepunch_retry_noop_for_non_relayed_peer(
            peer in arb_peer_id(),
        ) {
            let state = TunnelState::new();

            let (_, commands) = state.transition(Event::HolePunchRetryTick { peer });

            prop_assert!(commands.is_empty());
        }

        #[test]
        fn direct_connection_clears_relayed_after_retry(
            peer in arb_peer_id(),
        ) {
            let mut state = TunnelState::new();
            state.relayed_peers.insert(peer);
            state.holepunch_failed.insert(peer);

            let (state, _) = state.transition(Event::TunnelPeerConnected {
                peer, relayed: false,
            });

            prop_assert!(!state.relayed_peers.contains(&peer));
            prop_assert!(!state.holepunch_failed.contains(&peer));
        }

        // ─── Symmetric NAT skip tests ──────────────────────────────────

        #[test]
        fn relayed_peer_skips_holepunch_when_symmetric_nat(
            peer in arb_peer_id(),
            service in arb_service_name(),
            bind in arb_service_addr(),
        ) {
            let mut state = TunnelState::new();
            state.set_nat_mapping(NatMapping::AddressDependent);
            state.relayed_peers.insert(peer);
            state.add_peer_tunnel(peer, service.clone(), bind.clone());

            let (state, commands) = state.transition(Event::DhtPeerLookupComplete {
                peer, connected: true,
            });

            let has_spawn = commands.contains(&Command::SpawnTunnel {
                peer, service, bind, relayed: true,
            });
            prop_assert!(has_spawn);
            prop_assert!(!state.awaiting_holepunch.contains_key(&peer));
        }

        #[test]
        fn relayed_peer_awaits_holepunch_when_cone_nat(
            peer in arb_peer_id(),
            service in arb_service_name(),
            bind in arb_service_addr(),
        ) {
            let mut state = TunnelState::new();
            state.set_nat_mapping(NatMapping::EndpointIndependent);
            state.relayed_peers.insert(peer);
            state.add_peer_tunnel(peer, service.clone(), bind.clone());

            let (state, commands) = state.transition(Event::DhtPeerLookupComplete {
                peer, connected: true,
            });

            let has_await = commands.contains(&Command::AwaitHolePunch { peer });
            prop_assert!(has_await);
            prop_assert!(state.awaiting_holepunch.contains_key(&peer));
        }

        #[test]
        fn tunnel_connected_relayed_skips_holepunch_when_symmetric(
            peer in arb_peer_id(),
            service in arb_service_name(),
            bind in arb_service_addr(),
        ) {
            let mut state = TunnelState::new();
            state.set_nat_mapping(NatMapping::AddressDependent);
            state.add_peer_tunnel(peer, service.clone(), bind.clone());

            let (_, commands) = state.transition(Event::TunnelPeerConnected {
                peer, relayed: true,
            });

            let has_spawn = commands.contains(&Command::SpawnTunnel {
                peer, service, bind, relayed: true,
            });
            prop_assert!(has_spawn);
        }

        #[test]
        fn tunnel_connected_relayed_primes_without_pending_specs(
            peer in arb_peer_id(),
            addr in arb_multiaddr(),
        ) {
            let mut state = TunnelState::new();
            state.set_nat_mapping(NatMapping::EndpointIndependent);
            state.peer_external_addrs.insert(peer, vec![addr.clone()]);

            let (state, commands) = state.transition(Event::TunnelPeerConnected {
                peer, relayed: true,
            });

            let has_prime = commands.iter().any(|c| matches!(
                c, Command::PrimeNatMapping { peer: p, .. } if *p == peer
            ));
            prop_assert!(has_prime, "service-exposing side must prime NAT for bidirectional hole-punching");
            prop_assert!(state.relayed_peers.contains(&peer));
            prop_assert!(!state.awaiting_holepunch.contains_key(&peer));
        }

        #[test]
        fn tunnel_connected_relayed_no_prime_without_specs_when_symmetric(
            peer in arb_peer_id(),
        ) {
            let mut state = TunnelState::new();
            state.set_nat_mapping(NatMapping::AddressDependent);

            let (_, commands) = state.transition(Event::TunnelPeerConnected {
                peer, relayed: true,
            });

            prop_assert!(commands.is_empty());
        }

        #[test]
        fn nat_mapping_detected_updates_tunnel_state(
            mapping in prop_oneof![
                Just(NatMapping::EndpointIndependent),
                Just(NatMapping::AddressDependent),
                Just(NatMapping::Unknown),
            ],
        ) {
            let state = TunnelState::new();
            let (state, commands) = state.transition(Event::NatMappingDetected(mapping));
            prop_assert_eq!(state.nat_mapping, mapping);
            prop_assert!(commands.is_empty());
        }
    }
}
