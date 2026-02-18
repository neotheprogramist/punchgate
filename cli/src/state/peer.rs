use std::collections::{HashMap, HashSet};

use libp2p::{Multiaddr, PeerId, multiaddr::Protocol};

use super::{command::Command, event::Event};
use crate::traits::MealyMachine;

// ─── Address helpers ─────────────────────────────────────────────────────────

fn is_loopback_addr(addr: &Multiaddr) -> bool {
    addr.iter().any(|p| match p {
        Protocol::Ip4(ip) => ip.is_loopback(),
        Protocol::Ip6(ip) => ip.is_loopback(),
        _ => false,
    })
}

fn prefer_public_addr(addrs: &HashSet<Multiaddr>) -> Option<&Multiaddr> {
    addrs
        .iter()
        .find(|a| crate::external_addr::has_public_ip(a))
        .or_else(|| addrs.iter().find(|a| !is_loopback_addr(a)))
        .or_else(|| addrs.iter().next())
}

// ─── State types (product of orthogonal coproducts) ─────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, strum::Display)]
pub enum Phase {
    Joining,
    Ready,
    ShuttingDown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, strum::Display)]
#[strum(serialize_all = "lowercase")]
pub enum NatStatus {
    Unknown,
    Public,
    Private,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerState {
    pub phase: Phase,
    pub nat_status: NatStatus,
    pub nat_mapping: crate::nat_probe::NatMapping,
    pub relay_reserved: bool,
    pub known_peers: HashMap<PeerId, HashSet<Multiaddr>>,
    pub bootstrap_peers: HashSet<PeerId>,
    pub kad_bootstrapped: bool,
    pub external_addrs: HashSet<Multiaddr>,
}

impl Default for PeerState {
    fn default() -> Self {
        Self::new()
    }
}

impl PeerState {
    pub fn new() -> Self {
        Self {
            phase: Phase::Joining,
            nat_status: NatStatus::Unknown,
            nat_mapping: crate::nat_probe::NatMapping::Unknown,
            relay_reserved: false,
            known_peers: HashMap::new(),
            bootstrap_peers: HashSet::new(),
            kad_bootstrapped: false,
            external_addrs: HashSet::new(),
        }
    }

    fn maybe_request_relay(&self) -> Option<Command> {
        match (self.nat_status, self.relay_reserved) {
            (NatStatus::Private, false) => self
                .known_peers
                .iter()
                .find(|(p, _)| self.bootstrap_peers.contains(p))
                .and_then(|(&peer, addrs)| {
                    prefer_public_addr(addrs).map(|addr| Command::RequestRelayReservation {
                        relay_peer: peer,
                        relay_addr: addr.clone(),
                    })
                }),
            _ => None,
        }
    }

    fn maybe_become_ready(mut self) -> (Self, Vec<Command>) {
        match (
            self.phase,
            self.kad_bootstrapped,
            self.external_addrs.is_empty(),
        ) {
            (Phase::Joining, true, false) => {
                self.phase = Phase::Ready;
                (self, Vec::new())
            }
            (Phase::Joining, ..) | (Phase::Ready, ..) | (Phase::ShuttingDown, ..) => {
                (self, Vec::new())
            }
        }
    }
}

impl MealyMachine for PeerState {
    type Event = Event;
    type Command = Command;

    fn transition(mut self, event: Event) -> (Self, Vec<Command>) {
        let mut commands = Vec::new();

        match event {
            Event::ListeningOn { .. } => {}

            Event::BootstrapConnected { peer, addr } => {
                self.bootstrap_peers.insert(peer);
                self.known_peers
                    .entry(peer)
                    .or_default()
                    .insert(addr.clone());
                commands.push(Command::KademliaAddAddress { peer, addr });
                match self.phase {
                    Phase::Joining | Phase::Ready => {
                        commands.push(Command::KademliaBootstrap);
                    }
                    Phase::ShuttingDown => {}
                }
                // Relay request deferred to PeerIdentified — requesting relay on
                // BootstrapConnected races with the bootstrap's own Identify
                // exchange, arriving before the relay server learns its external
                // addresses (NoAddressesInReservation).
            }

            Event::KademliaBootstrapOk => {
                self.kad_bootstrapped = true;
                let (s, cmds) = self.maybe_become_ready();
                self = s;
                commands.extend(cmds);
            }

            Event::KademliaBootstrapFailed { .. } => {}

            Event::NatStatusChanged(status) => {
                self.nat_status = status;
                match (status, self.relay_reserved) {
                    (NatStatus::Private, false) => {
                        commands.extend(self.maybe_request_relay());
                    }
                    (NatStatus::Public, true) => {
                        self.relay_reserved = false;
                    }
                    (NatStatus::Public, false)
                    | (NatStatus::Private, true)
                    | (NatStatus::Unknown, _) => {}
                }
            }

            Event::NatMappingDetected(mapping) => {
                self.nat_mapping = mapping;
            }

            Event::DiscoveryTimeout => match (
                self.phase,
                self.kad_bootstrapped,
                self.known_peers.is_empty(),
            ) {
                (Phase::Joining, true, false) => {
                    self.phase = Phase::Ready;
                }
                (Phase::Joining, ..) | (Phase::Ready, ..) | (Phase::ShuttingDown, ..) => {}
            },

            Event::MdnsDiscovered { peers } => {
                for (peer, addr) in peers {
                    self.known_peers
                        .entry(peer)
                        .or_default()
                        .insert(addr.clone());
                    commands.push(Command::KademliaAddAddress { peer, addr });
                }
            }

            Event::MdnsExpired { peers } => {
                for (peer, addr) in peers {
                    let now_empty = self.known_peers.get_mut(&peer).map(|addrs| {
                        addrs.remove(&addr);
                        addrs.is_empty()
                    });
                    if let Some(true) = now_empty {
                        self.known_peers.remove(&peer);
                    }
                }
            }

            Event::PeerIdentified {
                peer, listen_addrs, ..
            } => {
                let entry = self.known_peers.entry(peer).or_default();
                for addr in &listen_addrs {
                    entry.insert(addr.clone());
                    if crate::external_addr::has_public_ip(addr) {
                        commands.push(Command::KademliaAddAddress {
                            peer,
                            addr: addr.clone(),
                        });
                    }
                }
                if self.bootstrap_peers.contains(&peer) {
                    commands.extend(self.maybe_request_relay());
                }
            }

            Event::RelayReservationAccepted { .. } => {
                self.relay_reserved = true;
            }

            Event::RelayReservationFailed { .. } => {
                self.relay_reserved = false;
                commands.extend(self.maybe_request_relay());
            }

            Event::HolePunchSucceeded { .. }
            | Event::HolePunchFailed { .. }
            | Event::HolePunchTimeout { .. }
            | Event::HolePunchRetryTick { .. }
            | Event::DhtPeerLookupComplete { .. }
            | Event::DhtServiceResolved { .. }
            | Event::DhtServiceFailed { .. }
            | Event::TunnelPeerConnected { .. }
            | Event::PhaseChanged { .. } => {}

            Event::NoBootstrapPeers => match self.phase {
                Phase::Joining => {
                    self.phase = Phase::Ready;
                }
                Phase::Ready | Phase::ShuttingDown => {}
            },

            Event::ExternalAddrConfirmed { addr } => {
                self.external_addrs.insert(addr);
                let (s, cmds) = self.maybe_become_ready();
                self = s;
                commands.extend(cmds);
            }

            Event::ExternalAddrExpired { addr } => {
                self.external_addrs.remove(&addr);
            }

            Event::ShutdownRequested => {
                self.phase = Phase::ShuttingDown;
                commands.push(Command::Shutdown);
            }

            Event::ConnectionLost {
                peer,
                remaining_connections,
            } => {
                if remaining_connections == 0 {
                    self.known_peers.remove(&peer);
                    self.relay_reserved = false;
                    match (
                        self.phase,
                        self.known_peers.is_empty(),
                        self.bootstrap_peers.is_empty(),
                    ) {
                        (Phase::Ready, true, false) => {
                            self.phase = Phase::Joining;
                            self.kad_bootstrapped = false;
                            commands.push(Command::KademliaBootstrap);
                        }
                        (Phase::Ready, true, true)
                        | (Phase::Ready, false, _)
                        | (Phase::Joining, ..)
                        | (Phase::ShuttingDown, ..) => {}
                    }
                }
            }
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
        arb_multiaddr, arb_nat_status, arb_nonloopback_multiaddr, arb_peer_id, arb_phase,
        arb_private_multiaddr, arb_public_multiaddr, arb_relay_circuit_multiaddr,
    };

    proptest! {
        #[test]
        fn shutdown_absorbs_any_phase(phase in arb_phase()) {
            let mut state = PeerState::new();
            state.phase = phase;
            let (new_state, commands) = state.transition(Event::ShutdownRequested);
            prop_assert_eq!(new_state.phase, Phase::ShuttingDown);
            prop_assert!(commands.contains(&Command::Shutdown));
        }

        #[test]
        fn bootstrap_stays_joining(
            peer in arb_peer_id(),
            addr in arb_multiaddr(),
        ) {
            let state = PeerState::new();
            let (new_state, commands) = state.transition(Event::BootstrapConnected {
                peer,
                addr: addr.clone(),
            });
            prop_assert_eq!(new_state.phase, Phase::Joining);
            prop_assert!(commands.contains(&Command::KademliaBootstrap));
            let has_kad_add = commands.contains(&Command::KademliaAddAddress { peer, addr });
            prop_assert!(has_kad_add);
            prop_assert!(new_state.known_peers.contains_key(&peer));
            prop_assert!(new_state.bootstrap_peers.contains(&peer));
        }

        #[test]
        fn second_bootstrap_reissues_kad_bootstrap(
            peer1 in arb_peer_id(),
            peer2 in arb_peer_id(),
            addr1 in arb_multiaddr(),
            addr2 in arb_multiaddr(),
        ) {
            let state = PeerState::new();
            let (state, _) = state.transition(Event::BootstrapConnected {
                peer: peer1,
                addr: addr1,
            });
            prop_assert_eq!(state.phase, Phase::Joining);

            let (state, commands) = state.transition(Event::BootstrapConnected {
                peer: peer2,
                addr: addr2,
            });
            prop_assert_eq!(state.phase, Phase::Joining);
            prop_assert!(commands.contains(&Command::KademliaBootstrap));
        }

        #[test]
        fn kad_completion_needs_external_addrs_for_ready(
            peer in arb_peer_id(),
            addr in arb_multiaddr(),
            external_addr in arb_multiaddr(),
        ) {
            let state = PeerState::new();
            let (state, _) = state.transition(Event::BootstrapConnected {
                peer, addr,
            });
            prop_assert_eq!(state.phase, Phase::Joining);

            // KademliaBootstrapOk alone stays Joining (no external addrs yet)
            let (state, _) = state.transition(Event::KademliaBootstrapOk);
            prop_assert_eq!(state.phase, Phase::Joining);
            prop_assert!(state.kad_bootstrapped);

            // ExternalAddrConfirmed triggers Ready
            let (state, _) = state.transition(Event::ExternalAddrConfirmed {
                addr: external_addr,
            });
            prop_assert_eq!(state.phase, Phase::Ready);
        }

        #[test]
        fn external_addr_confirmed_triggers_ready(
            peer in arb_peer_id(),
            addr in arb_multiaddr(),
            external_addr in arb_multiaddr(),
        ) {
            let state = PeerState::new();
            let (state, _) = state.transition(Event::BootstrapConnected { peer, addr });

            // ExternalAddrConfirmed without kad_bootstrapped stays Joining
            let (state, _) = state.transition(Event::ExternalAddrConfirmed {
                addr: external_addr.clone(),
            });
            prop_assert_eq!(state.phase, Phase::Joining);
            prop_assert!(state.external_addrs.contains(&external_addr));

            // KademliaBootstrapOk with external addrs triggers Ready
            let (state, _) = state.transition(Event::KademliaBootstrapOk);
            prop_assert_eq!(state.phase, Phase::Ready);
        }

        #[test]
        fn discovery_timeout_enters_ready_with_kad_and_peers(
            peer in arb_peer_id(),
            addr in arb_multiaddr(),
        ) {
            let mut state = PeerState::new();
            state.phase = Phase::Joining;
            state.kad_bootstrapped = true;
            state.known_peers.entry(peer).or_default().insert(addr);

            let (state, _commands) = state.transition(Event::DiscoveryTimeout);
            prop_assert_eq!(state.phase, Phase::Ready);
        }

        #[test]
        fn discovery_timeout_with_no_peers_stays_joining(
            nat_status in arb_nat_status(),
        ) {
            let mut state = PeerState::new();
            state.phase = Phase::Joining;
            state.nat_status = nat_status;

            let (state, _commands) = state.transition(Event::DiscoveryTimeout);
            prop_assert_eq!(state.phase, Phase::Joining);
        }

        #[test]
        fn bootstrap_reconnect_in_ready_refreshes(
            peer in arb_peer_id(),
            addr in arb_multiaddr(),
        ) {
            let mut state = PeerState::new();
            state.phase = Phase::Ready;
            state.bootstrap_peers.insert(peer);

            let (state, commands) = state.transition(Event::BootstrapConnected {
                peer, addr,
            });
            prop_assert_eq!(state.phase, Phase::Ready);
            prop_assert!(commands.contains(&Command::KademliaBootstrap));
        }

        #[test]
        fn nat_status_always_updates(
            phase in arb_phase(),
            new_status in arb_nat_status(),
        ) {
            let mut state = PeerState::new();
            state.phase = phase;
            let (new_state, _) = state.transition(Event::NatStatusChanged(new_status));
            prop_assert_eq!(new_state.nat_status, new_status);
        }

        #[test]
        fn nat_transitions_symmetric(
            from_status in arb_nat_status(),
            to_status in arb_nat_status(),
        ) {
            let mut state = PeerState::new();
            state.phase = Phase::Joining;
            let (state, _) = state.transition(Event::NatStatusChanged(from_status));
            prop_assert_eq!(state.nat_status, from_status);

            let (state, _) = state.transition(Event::NatStatusChanged(to_status));
            prop_assert_eq!(state.nat_status, to_status);
        }

        #[test]
        fn nat_mapping_detected_stores_mapping(
            mapping in prop_oneof![
                Just(crate::nat_probe::NatMapping::EndpointIndependent),
                Just(crate::nat_probe::NatMapping::AddressDependent),
                Just(crate::nat_probe::NatMapping::Unknown),
            ],
        ) {
            let state = PeerState::new();
            let (state, _) = state.transition(Event::NatMappingDetected(mapping));
            prop_assert_eq!(state.nat_mapping, mapping);
        }

        #[test]
        fn nat_private_requests_relay(
            peer in arb_peer_id(),
            addr in arb_nonloopback_multiaddr(),
        ) {
            let mut state = PeerState::new();
            state.phase = Phase::Joining;
            state.bootstrap_peers.insert(peer);
            state.known_peers.entry(peer).or_default().insert(addr);

            let (state, commands) = state.transition(Event::NatStatusChanged(NatStatus::Private));
            prop_assert_eq!(state.nat_status, NatStatus::Private);
            let has_relay_cmd = commands.iter().any(|c| matches!(
                c,
                Command::RequestRelayReservation { relay_peer, .. }
                if *relay_peer == peer
            ));
            prop_assert!(has_relay_cmd);
        }

        #[test]
        fn mdns_discovered_preserves_phase_and_nat(
            phase in arb_phase(),
            nat_status in arb_nat_status(),
            peer in arb_peer_id(),
            addr in arb_multiaddr(),
        ) {
            let mut state = PeerState::new();
            state.phase = phase;
            state.nat_status = nat_status;

            let (new_state, commands) = state.transition(Event::MdnsDiscovered {
                peers: vec![(peer, addr.clone())],
            });

            prop_assert_eq!(new_state.phase, phase);
            prop_assert_eq!(new_state.nat_status, nat_status);
            prop_assert!(new_state.known_peers.contains_key(&peer));
            let has_kad_add = commands.contains(&Command::KademliaAddAddress { peer, addr });
            prop_assert!(has_kad_add);
        }

        #[test]
        fn mdns_expired_removes_addresses(
            peer in arb_peer_id(),
            addr1 in arb_multiaddr(),
            addr2 in arb_multiaddr(),
        ) {
            let mut state = PeerState::new();
            state.known_peers.entry(peer).or_default().insert(addr1.clone());
            state.known_peers.entry(peer).or_default().insert(addr2.clone());

            let (state, _) = state.transition(Event::MdnsExpired {
                peers: vec![(peer, addr1.clone())],
            });

            match addr1 == addr2 {
                true => {
                    prop_assert!(!state.known_peers.contains_key(&peer));
                }
                false => {
                    prop_assert!(state.known_peers.contains_key(&peer));

                    let (state, _) = state.transition(Event::MdnsExpired {
                        peers: vec![(peer, addr2)],
                    });
                    prop_assert!(!state.known_peers.contains_key(&peer));
                }
            }
        }

        #[test]
        fn no_bootstrap_peers_enters_ready(phase in arb_phase()) {
            let mut state = PeerState::new();
            state.phase = phase;

            let (new_state, _commands) = state.transition(Event::NoBootstrapPeers);
            match phase {
                Phase::Joining => {
                    prop_assert_eq!(new_state.phase, Phase::Ready);
                }
                Phase::Ready | Phase::ShuttingDown => {
                    prop_assert_eq!(new_state.phase, phase);
                }
            }
        }

        #[test]
        fn external_addr_confirmed_adds_to_set(
            addr in arb_multiaddr(),
        ) {
            let state = PeerState::new();
            let (state, _) = state.transition(Event::ExternalAddrConfirmed { addr: addr.clone() });
            prop_assert!(state.external_addrs.contains(&addr));

            let (state, _) = state.transition(Event::ExternalAddrConfirmed { addr: addr.clone() });
            prop_assert!(state.external_addrs.contains(&addr));
            prop_assert_eq!(state.external_addrs.len(), 1);
        }

        #[test]
        fn external_addr_expired_removes_from_set(
            addr in arb_multiaddr(),
        ) {
            let mut state = PeerState::new();
            state.external_addrs.insert(addr.clone());

            let (state, _) = state.transition(Event::ExternalAddrExpired { addr: addr.clone() });
            prop_assert!(!state.external_addrs.contains(&addr));
        }

        #[test]
        fn relay_accepted_sets_reserved(relay_peer in arb_peer_id()) {
            let mut state = PeerState::new();
            state.relay_reserved = false;

            let (new_state, _) = state.transition(Event::RelayReservationAccepted { relay_peer });
            prop_assert!(new_state.relay_reserved);
        }

        #[test]
        fn relay_failed_clears_reserved(
            relay_peer in arb_peer_id(),
            reason in "[a-z]{1,20}",
        ) {
            let mut state = PeerState::new();
            state.phase = Phase::Joining;
            state.relay_reserved = false;

            let (new_state, _) = state.transition(Event::RelayReservationFailed {
                relay_peer,
                reason,
            });
            prop_assert!(!new_state.relay_reserved);
        }

        #[test]
        fn public_nat_clears_relay(relay_peer in arb_peer_id()) {
            let mut state = PeerState::new();
            state.phase = Phase::Ready;
            state.relay_reserved = true;
            state.kad_bootstrapped = true;
            state.known_peers.entry(relay_peer).or_default();

            let (new_state, _) = state.transition(Event::NatStatusChanged(NatStatus::Public));
            prop_assert!(!new_state.relay_reserved);
        }

        #[test]
        fn peer_identified_adds_public_addresses_to_kademlia(
            peer in arb_peer_id(),
            addrs in proptest::collection::vec(arb_public_multiaddr(), 1..5),
            observed in arb_multiaddr(),
        ) {
            let state = PeerState::new();
            let (new_state, commands) = state.transition(Event::PeerIdentified {
                peer,
                listen_addrs: addrs.clone(),
                observed_addr: observed,
            });

            let stored = new_state.known_peers.get(&peer)
                .expect("peer should be in known_peers after PeerIdentified");
            for addr in &addrs {
                prop_assert!(stored.contains(addr));
            }

            let kad_add_count = commands.iter()
                .filter(|c| matches!(c, Command::KademliaAddAddress { .. }))
                .count();
            prop_assert_eq!(kad_add_count, addrs.len());
        }

        #[test]
        fn peer_identified_filters_private_addrs_from_kademlia(
            peer in arb_peer_id(),
            public_addr in arb_public_multiaddr(),
            private_addr in arb_private_multiaddr(),
            observed in arb_multiaddr(),
        ) {
            let state = PeerState::new();
            let (new_state, commands) = state.transition(Event::PeerIdentified {
                peer,
                listen_addrs: vec![public_addr.clone(), private_addr.clone()],
                observed_addr: observed,
            });

            // Both addresses stored in known_peers
            let stored = new_state.known_peers.get(&peer)
                .expect("peer should be in known_peers after PeerIdentified");
            prop_assert!(stored.contains(&public_addr));
            prop_assert!(stored.contains(&private_addr));

            // Only public address pushed to Kademlia
            let kad_addrs: Vec<_> = commands.iter()
                .filter_map(|c| match c {
                    Command::KademliaAddAddress { addr, .. } => Some(addr),
                    _ => None,
                })
                .collect();
            prop_assert_eq!(kad_addrs.len(), 1);
            prop_assert_eq!(kad_addrs[0], &public_addr);
        }

        #[test]
        fn connection_lost_removes_peer_and_regresses(
            peer in arb_peer_id(),
            addr in arb_multiaddr(),
        ) {
            let mut state = PeerState::new();
            state.phase = Phase::Ready;
            state.kad_bootstrapped = true;
            state.bootstrap_peers.insert(peer);
            state.known_peers.entry(peer).or_default().insert(addr);

            let (new_state, commands) = state.transition(Event::ConnectionLost {
                peer,
                remaining_connections: 0,
            });

            prop_assert!(!new_state.known_peers.contains_key(&peer));
            prop_assert_eq!(new_state.phase, Phase::Joining);
            prop_assert!(!new_state.kad_bootstrapped);
            prop_assert!(commands.contains(&Command::KademliaBootstrap));
        }

        #[test]
        fn connection_lost_standalone_stays_ready(
            peer in arb_peer_id(),
            addr in arb_multiaddr(),
        ) {
            let mut state = PeerState::new();
            state.phase = Phase::Ready;
            state.kad_bootstrapped = true;
            state.known_peers.entry(peer).or_default().insert(addr);

            let (new_state, commands) = state.transition(Event::ConnectionLost {
                peer,
                remaining_connections: 0,
            });

            prop_assert!(!new_state.known_peers.contains_key(&peer));
            prop_assert_eq!(new_state.phase, Phase::Ready);
            prop_assert!(new_state.kad_bootstrapped);
            prop_assert!(!commands.contains(&Command::KademliaBootstrap));
        }

        #[test]
        fn connection_lost_with_remaining_keeps_peer(
            peer in arb_peer_id(),
            addr in arb_multiaddr(),
            remaining in 1u32..100,
        ) {
            let mut state = PeerState::new();
            state.phase = Phase::Ready;
            state.known_peers.entry(peer).or_default().insert(addr);

            let (new_state, _) = state.transition(Event::ConnectionLost {
                peer,
                remaining_connections: remaining,
            });
            prop_assert!(new_state.known_peers.contains_key(&peer));
            prop_assert_eq!(new_state.phase, Phase::Ready);
        }

        #[test]
        fn listening_on_loopback_is_noop(
            phase in arb_phase(),
            port in 1024u16..65535u16,
        ) {
            let mut state = PeerState::new();
            state.phase = phase;

            // Infallible: formatted loopback multiaddr is always valid
            let addr: Multiaddr = format!("/ip4/127.0.0.1/tcp/{port}")
                .parse()
                .expect("loopback multiaddr is always valid");
            let (_, commands) = state.transition(Event::ListeningOn { addr });
            prop_assert!(commands.is_empty());
        }

        #[test]
        fn listening_on_nonloopback_is_noop(
            phase in arb_phase(),
            addr in arb_nonloopback_multiaddr(),
        ) {
            let mut state = PeerState::new();
            state.phase = phase;

            let (_, commands) = state.transition(Event::ListeningOn { addr });
            prop_assert!(commands.is_empty());
        }

        #[test]
        fn listening_on_circuit_is_noop(
            phase in arb_phase(),
            addr in arb_relay_circuit_multiaddr(),
        ) {
            let mut state = PeerState::new();
            state.phase = phase;

            let (_, commands) = state.transition(Event::ListeningOn { addr });
            prop_assert!(commands.is_empty());
        }

        #[test]
        fn log_only_events_preserve_state(
            phase in arb_phase(),
            nat_status in arb_nat_status(),
            peer in arb_peer_id(),
            reason in "[a-z ]{1,20}",
        ) {
            let mut state = PeerState::new();
            state.phase = phase;
            state.nat_status = nat_status;

            let (s1, c1) = state.clone().transition(Event::HolePunchSucceeded { remote_peer: peer });
            prop_assert_eq!(s1.phase, phase);
            prop_assert_eq!(s1.nat_status, nat_status);
            prop_assert!(c1.is_empty());

            let (s2, c2) = state.clone().transition(Event::HolePunchFailed {
                remote_peer: peer, reason: reason.clone(),
            });
            prop_assert_eq!(s2.phase, phase);
            prop_assert_eq!(s2.nat_status, nat_status);
            prop_assert!(c2.is_empty());

            let (s3, c3) = state.clone().transition(Event::DhtPeerLookupComplete {
                peer, connected: true,
            });
            prop_assert_eq!(s3.phase, phase);
            prop_assert_eq!(s3.nat_status, nat_status);
            prop_assert!(c3.is_empty());

            let (s4, c4) = state.clone().transition(Event::TunnelPeerConnected {
                peer, relayed: false,
            });
            prop_assert_eq!(s4.phase, phase);
            prop_assert_eq!(s4.nat_status, nat_status);
            prop_assert!(c4.is_empty());
        }

        #[test]
        fn kad_ok_without_external_addrs_stays_joining(
            nat_status in arb_nat_status(),
        ) {
            let mut state = PeerState::new();
            state.phase = Phase::Joining;
            state.nat_status = nat_status;

            let (state, _commands) = state.transition(Event::KademliaBootstrapOk);
            prop_assert_eq!(state.phase, Phase::Joining);
            prop_assert!(state.kad_bootstrapped);
        }

        #[test]
        fn bootstrap_reconnect_in_joining_stays_joining(
            peer in arb_peer_id(),
            addr in arb_multiaddr(),
        ) {
            let mut state = PeerState::new();
            state.phase = Phase::Joining;
            state.kad_bootstrapped = true;
            state.bootstrap_peers.insert(peer);

            let (state, commands) = state.transition(Event::BootstrapConnected {
                peer, addr,
            });
            // Still Joining because no external addrs confirmed
            prop_assert_eq!(state.phase, Phase::Joining);
            prop_assert!(commands.contains(&Command::KademliaBootstrap));
        }

        #[test]
        fn relay_addr_prefers_public_over_loopback(
            peer in arb_peer_id(),
            public_addr in arb_public_multiaddr(),
            port in 1024u16..65535u16,
        ) {
            // Infallible: formatted loopback multiaddr is always valid
            let loopback_addr: Multiaddr = format!("/ip4/127.0.0.1/udp/{port}/quic-v1")
                .parse()
                .expect("loopback multiaddr is always valid");

            let mut state = PeerState::new();
            state.phase = Phase::Joining;
            state.bootstrap_peers.insert(peer);
            let addrs = state.known_peers.entry(peer).or_default();
            addrs.insert(loopback_addr);
            addrs.insert(public_addr.clone());

            let (_, commands) = state.transition(Event::NatStatusChanged(NatStatus::Private));

            let relay_addr = commands.iter().find_map(|c| match c {
                Command::RequestRelayReservation { relay_addr, .. } => Some(relay_addr),
                _ => None,
            });
            if let Some(addr) = relay_addr {
                prop_assert!(
                    crate::external_addr::has_public_ip(addr),
                    "relay should use public addr, got {addr}"
                );
            }
        }

        #[test]
        fn relay_addr_prefers_public_over_private(
            peer in arb_peer_id(),
            public_addr in arb_public_multiaddr(),
            private_addr in arb_private_multiaddr(),
        ) {
            let mut state = PeerState::new();
            state.phase = Phase::Joining;
            state.bootstrap_peers.insert(peer);
            let addrs = state.known_peers.entry(peer).or_default();
            addrs.insert(private_addr);
            addrs.insert(public_addr.clone());

            let (_, commands) = state.transition(Event::NatStatusChanged(NatStatus::Private));

            let relay_addr = commands.iter().find_map(|c| match c {
                Command::RequestRelayReservation { relay_addr, .. } => Some(relay_addr),
                _ => None,
            });
            if let Some(addr) = relay_addr {
                prop_assert!(
                    crate::external_addr::has_public_ip(addr),
                    "relay should prefer public over private, got {addr}"
                );
            }
        }

        #[test]
        fn connection_lost_resets_relay_reserved(
            peer in arb_peer_id(),
            addr in arb_multiaddr(),
            other_peer in arb_peer_id(),
            other_addr in arb_multiaddr(),
        ) {
            prop_assume!(peer != other_peer);

            let mut state = PeerState::new();
            state.phase = Phase::Ready;
            state.relay_reserved = true;
            state.known_peers.entry(peer).or_default().insert(addr);
            state.known_peers.entry(other_peer).or_default().insert(other_addr);

            let (state, _) = state.transition(Event::ConnectionLost {
                peer,
                remaining_connections: 0,
            });

            prop_assert!(!state.relay_reserved);
            prop_assert_eq!(state.phase, Phase::Ready);
        }
    }
}
