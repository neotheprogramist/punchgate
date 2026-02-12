use std::collections::{HashMap, HashSet};

use libp2p::PeerId;

use super::{command::Command, event::Event, peer::Phase};
use crate::{
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
    services_published: bool,
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
            services_published: false,
        }
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
        let mut commands = Vec::new();

        for &peer in self.pending_by_peer.keys() {
            commands.push(Command::DhtLookupPeer { peer });
        }

        for service_name in self.pending_by_service.keys() {
            commands.push(Command::DhtGetProviders {
                service_name: service_name.clone(),
                key: KademliaKey::for_service(service_name),
            });
        }

        commands
    }
}

impl MealyMachine for TunnelState {
    type Event = Event;
    type Command = Command;

    fn transition(mut self, event: Event) -> (Self, Vec<Command>) {
        let mut commands = Vec::new();

        match event {
            Event::PhaseChanged {
                new: Phase::Participating,
                ..
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
                        for (service, bind) in specs {
                            commands.push(Command::SpawnTunnel {
                                peer,
                                service,
                                bind,
                            });
                        }
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
                            for (service, bind) in specs {
                                commands.push(Command::SpawnTunnel {
                                    peer: provider,
                                    service,
                                    bind,
                                });
                            }
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
                        if let Some(specs) = self.pending_by_peer.remove(&peer) {
                            self.awaiting_holepunch.insert(peer, specs);
                        }
                    }
                    false => {
                        if let Some(specs) = self.pending_by_peer.remove(&peer) {
                            for (service, bind) in specs {
                                commands.push(Command::SpawnTunnel {
                                    peer,
                                    service,
                                    bind,
                                });
                            }
                        }
                    }
                }
            }

            Event::HolePunchSucceeded { remote_peer } => {
                if let Some(specs) = self.awaiting_holepunch.remove(&remote_peer) {
                    for (service, bind) in specs {
                        commands.push(Command::SpawnTunnel {
                            peer: remote_peer,
                            service,
                            bind,
                        });
                    }
                }
            }

            Event::HolePunchFailed { remote_peer, .. } => {
                self.awaiting_holepunch.remove(&remote_peer);
            }

            Event::PhaseChanged { .. }
            | Event::ListeningOn { .. }
            | Event::BootstrapConnected { .. }
            | Event::ShutdownRequested
            | Event::KademliaBootstrapOk
            | Event::KademliaBootstrapFailed { .. }
            | Event::MdnsDiscovered { .. }
            | Event::MdnsExpired { .. }
            | Event::PeerIdentified { .. }
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
        fn phase_changed_to_participating_publishes(
            old in arb_phase(),
        ) {
            prop_assume!(old != Phase::Participating);
            let state = TunnelState::new();
            let (_, commands) = state.transition(Event::PhaseChanged {
                old,
                new: Phase::Participating,
            });
            prop_assert!(commands.contains(&Command::PublishServices));
        }

        #[test]
        fn phase_changed_to_participating_initiates_lookups(
            old in arb_phase(),
            peer in arb_peer_id(),
            service in arb_service_name(),
            bind in arb_service_addr(),
            svc_name in arb_service_name(),
            svc_bind in arb_service_addr(),
        ) {
            prop_assume!(old != Phase::Participating);
            let mut state = TunnelState::new();
            state.add_peer_tunnel(peer, service, bind);
            state.add_service_tunnel(svc_name.clone(), svc_bind);

            let (_, commands) = state.transition(Event::PhaseChanged {
                old,
                new: Phase::Participating,
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
        fn phase_changed_non_participating_is_noop(
            old in arb_phase(),
            new in arb_phase(),
        ) {
            prop_assume!(new != Phase::Participating);
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
                peer, service, bind,
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
                peer: provider, service, bind,
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
                peer, service, bind,
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

            prop_assert!(commands.is_empty());
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
                peer, service, bind,
            });
            prop_assert!(has_spawn);
            prop_assert!(!state.awaiting_holepunch.contains_key(&peer));
        }

        #[test]
        fn hole_punch_failed_removes_pending(
            peer in arb_peer_id(),
            service in arb_service_name(),
            bind in arb_service_addr(),
            reason in "[a-z ]{1,20}",
        ) {
            let mut state = TunnelState::new();
            state.awaiting_holepunch.insert(peer, vec![(service, bind)]);

            let (state, commands) = state.transition(Event::HolePunchFailed {
                remote_peer: peer, reason,
            });

            prop_assert!(commands.is_empty());
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

            let (_, c) = state.clone().transition(Event::ConnectionLost {
                peer, remaining_connections: 0,
            });
            prop_assert!(c.is_empty());
        }
    }
}
