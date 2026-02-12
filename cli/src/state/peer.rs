use std::{
    collections::{HashMap, HashSet},
    str::FromStr,
};

use libp2p::{Multiaddr, PeerId, multiaddr::Protocol};
use thiserror::Error;

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

fn prefer_non_loopback(addrs: &HashSet<Multiaddr>) -> Option<&Multiaddr> {
    addrs
        .iter()
        .find(|a| !is_loopback_addr(a))
        .or_else(|| addrs.iter().next())
}

// ─── State types (product of orthogonal coproducts) ─────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, strum::Display)]
pub enum Phase {
    Initializing,
    Discovering,
    Participating,
    ShuttingDown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, strum::Display)]
#[strum(serialize_all = "lowercase")]
pub enum NatStatus {
    Unknown,
    Public,
    Private,
}

#[derive(Debug, Error)]
#[error("invalid NAT status '{0}': must be 'private' or 'public'")]
pub struct NatStatusParseError(String);

impl FromStr for NatStatus {
    type Err = NatStatusParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "private" => Ok(NatStatus::Private),
            "public" => Ok(NatStatus::Public),
            other => Err(NatStatusParseError(other.to_string())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayState {
    Idle,
    Requesting { relay_peer: PeerId },
    Reserved { relay_peer: PeerId },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerState {
    pub phase: Phase,
    pub nat_status: NatStatus,
    pub relay: RelayState,
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
            phase: Phase::Initializing,
            nat_status: NatStatus::Unknown,
            relay: RelayState::Idle,
            known_peers: HashMap::new(),
            bootstrap_peers: HashSet::new(),
            kad_bootstrapped: false,
            external_addrs: HashSet::new(),
        }
    }

    fn maybe_enter_participating(mut self) -> (Self, Vec<Command>) {
        let mut commands = Vec::new();
        match (
            self.phase,
            self.kad_bootstrapped,
            self.known_peers.is_empty(),
        ) {
            (Phase::Discovering, true, false) => {
                self.phase = Phase::Participating;

                let relay_candidate = match &self.relay {
                    RelayState::Idle => self
                        .known_peers
                        .iter()
                        .find(|(p, _)| self.bootstrap_peers.contains(p))
                        .and_then(|(&peer, addrs)| {
                            prefer_non_loopback(addrs).map(|addr| (peer, addr.clone()))
                        }),
                    RelayState::Requesting { .. } | RelayState::Reserved { .. } => None,
                };

                if let Some((relay_peer, relay_addr)) = relay_candidate {
                    self.relay = RelayState::Requesting { relay_peer };
                    commands.push(Command::RequestRelayReservation {
                        relay_peer,
                        relay_addr,
                    });
                }
            }
            (Phase::Discovering, ..)
            | (Phase::Initializing, ..)
            | (Phase::Participating, ..)
            | (Phase::ShuttingDown, ..) => {}
        }
        (self, commands)
    }
}

impl MealyMachine for PeerState {
    type Event = Event;
    type Command = Command;

    fn transition(mut self, event: Event) -> (Self, Vec<Command>) {
        let mut commands = Vec::new();

        match event {
            Event::ListeningOn { addr } => match is_loopback_addr(&addr) {
                true => {}
                false => commands.push(Command::AddExternalAddress(addr)),
            },

            Event::BootstrapConnected { peer, addr } => {
                self.bootstrap_peers.insert(peer);
                self.known_peers
                    .entry(peer)
                    .or_default()
                    .insert(addr.clone());
                commands.push(Command::KademliaAddAddress {
                    peer,
                    addr: addr.clone(),
                });

                match self.phase {
                    Phase::Initializing => {
                        self.phase = Phase::Discovering;
                        commands.push(Command::KademliaBootstrap);
                    }
                    Phase::Discovering => {
                        commands.push(Command::KademliaBootstrap);
                        let (s, cmds) = self.maybe_enter_participating();
                        self = s;
                        commands.extend(cmds);
                    }
                    Phase::Participating => {
                        commands.push(Command::KademliaBootstrap);
                        match &self.relay {
                            RelayState::Idle => {
                                if let Some(relay_addr) = self
                                    .known_peers
                                    .get(&peer)
                                    .and_then(|addrs| prefer_non_loopback(addrs))
                                {
                                    self.relay = RelayState::Requesting { relay_peer: peer };
                                    commands.push(Command::RequestRelayReservation {
                                        relay_peer: peer,
                                        relay_addr: relay_addr.clone(),
                                    });
                                }
                            }
                            RelayState::Requesting { .. } | RelayState::Reserved { .. } => {}
                        }
                    }
                    Phase::ShuttingDown => {}
                }
            }

            Event::KademliaBootstrapOk => {
                self.kad_bootstrapped = true;
                let (s, cmds) = self.maybe_enter_participating();
                self = s;
                commands.extend(cmds);
            }

            Event::KademliaBootstrapFailed { .. } => {}

            Event::NatStatusChanged(status) => {
                self.nat_status = status;
                match (status, &self.relay) {
                    (NatStatus::Public, RelayState::Reserved { .. }) => {
                        self.relay = RelayState::Idle;
                    }
                    (NatStatus::Public, RelayState::Idle | RelayState::Requesting { .. })
                    | (NatStatus::Private | NatStatus::Unknown, _) => {}
                }
            }

            Event::DiscoveryTimeout => match (self.phase, self.known_peers.is_empty()) {
                (Phase::Discovering, false) => {
                    self.phase = Phase::Participating;
                }
                (Phase::Discovering, true)
                | (Phase::Initializing | Phase::Participating | Phase::ShuttingDown, _) => {}
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

            Event::PeerIdentified { peer, listen_addrs } => {
                let entry = self.known_peers.entry(peer).or_default();
                for addr in &listen_addrs {
                    entry.insert(addr.clone());
                    commands.push(Command::KademliaAddAddress {
                        peer,
                        addr: addr.clone(),
                    });
                }
            }

            Event::RelayReservationAccepted { relay_peer } => {
                self.relay = RelayState::Reserved { relay_peer };
            }

            Event::RelayReservationFailed { relay_peer, .. } => match &self.relay {
                RelayState::Requesting { relay_peer: rp } if *rp == relay_peer => {
                    self.relay = RelayState::Idle;
                }
                RelayState::Requesting { .. } | RelayState::Idle | RelayState::Reserved { .. } => {}
            },

            Event::HolePunchSucceeded { .. }
            | Event::HolePunchFailed { .. }
            | Event::DhtPeerLookupComplete { .. }
            | Event::DhtServiceResolved { .. }
            | Event::DhtServiceFailed { .. }
            | Event::TunnelPeerConnected { .. }
            | Event::PhaseChanged { .. } => {}

            Event::NoBootstrapPeers => match self.phase {
                Phase::Initializing => {
                    self.phase = Phase::Participating;
                }
                Phase::Discovering | Phase::Participating | Phase::ShuttingDown => {}
            },

            Event::ExternalAddrConfirmed { addr } => {
                self.external_addrs.insert(addr);
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

                    match &self.relay {
                        RelayState::Requesting { relay_peer }
                        | RelayState::Reserved { relay_peer }
                            if *relay_peer == peer =>
                        {
                            self.relay = RelayState::Idle;
                        }
                        RelayState::Requesting { .. }
                        | RelayState::Reserved { .. }
                        | RelayState::Idle => {}
                    }

                    match (
                        self.phase,
                        self.known_peers.is_empty(),
                        self.bootstrap_peers.is_empty(),
                    ) {
                        (Phase::Participating, true, false) => {
                            self.phase = Phase::Discovering;
                            self.kad_bootstrapped = false;
                            commands.push(Command::KademliaBootstrap);
                        }
                        (Phase::Participating, true, true)
                        | (Phase::Participating, false, _)
                        | (Phase::Initializing, ..)
                        | (Phase::Discovering, ..)
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
        arb_relay_circuit_multiaddr,
    };

    #[test]
    fn initial_state_invariants() {
        let state = PeerState::new();
        assert_eq!(state.phase, Phase::Initializing);
        assert_eq!(state.nat_status, NatStatus::Unknown);
        assert_eq!(state.relay, RelayState::Idle);
        assert!(state.known_peers.is_empty());
        assert!(state.bootstrap_peers.is_empty());
        assert!(!state.kad_bootstrapped);
        assert!(state.external_addrs.is_empty());
    }

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
        fn bootstrap_enters_discovering(
            peer in arb_peer_id(),
            addr in arb_multiaddr(),
        ) {
            let state = PeerState::new();
            let (new_state, commands) = state.transition(Event::BootstrapConnected {
                peer,
                addr: addr.clone(),
            });
            prop_assert_eq!(new_state.phase, Phase::Discovering);
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
            prop_assert_eq!(state.phase, Phase::Discovering);

            let (state, commands) = state.transition(Event::BootstrapConnected {
                peer: peer2,
                addr: addr2,
            });
            prop_assert_eq!(state.phase, Phase::Discovering);
            prop_assert!(commands.contains(&Command::KademliaBootstrap));
        }

        #[test]
        fn kad_completion_enters_participating(
            peer in arb_peer_id(),
            addr in arb_multiaddr(),
        ) {
            let state = PeerState::new();
            let (state, _) = state.transition(Event::BootstrapConnected {
                peer, addr,
            });
            prop_assert_eq!(state.phase, Phase::Discovering);

            let (state, _commands) = state.transition(Event::KademliaBootstrapOk);
            prop_assert_eq!(state.phase, Phase::Participating);
        }

        #[test]
        fn discovery_timeout_enters_participating_with_peers(
            peer in arb_peer_id(),
            addr in arb_multiaddr(),
        ) {
            let state = PeerState::new();
            let (state, _) = state.transition(Event::BootstrapConnected { peer, addr });
            prop_assert_eq!(state.phase, Phase::Discovering);

            let (state, _commands) = state.transition(Event::DiscoveryTimeout);
            prop_assert_eq!(state.phase, Phase::Participating);
        }

        #[test]
        fn discovery_timeout_with_no_peers_stays_discovering(
            nat_status in arb_nat_status(),
        ) {
            let mut state = PeerState::new();
            state.phase = Phase::Discovering;
            state.nat_status = nat_status;

            let (state, _commands) = state.transition(Event::DiscoveryTimeout);
            prop_assert_eq!(state.phase, Phase::Discovering);
        }

        #[test]
        fn bootstrap_reconnect_in_participating_refreshes(
            peer in arb_peer_id(),
            addr in arb_multiaddr(),
        ) {
            let mut state = PeerState::new();
            state.phase = Phase::Participating;
            state.relay = RelayState::Idle;
            state.bootstrap_peers.insert(peer);

            let (state, commands) = state.transition(Event::BootstrapConnected {
                peer, addr,
            });
            prop_assert_eq!(state.phase, Phase::Participating);
            prop_assert!(commands.contains(&Command::KademliaBootstrap));
            let has_relay_cmd = commands.iter().any(|c| matches!(
                c,
                Command::RequestRelayReservation { relay_peer, .. }
                if *relay_peer == peer
            ));
            prop_assert!(has_relay_cmd);
            prop_assert_eq!(state.relay, RelayState::Requesting { relay_peer: peer });
        }

        #[test]
        fn bootstrap_reconnect_in_participating_skips_relay_if_reserved(
            peer in arb_peer_id(),
            other_peer in arb_peer_id(),
            addr in arb_multiaddr(),
        ) {
            prop_assume!(peer != other_peer);
            let mut state = PeerState::new();
            state.phase = Phase::Participating;
            state.relay = RelayState::Reserved { relay_peer: other_peer };
            state.bootstrap_peers.insert(peer);

            let (state, commands) = state.transition(Event::BootstrapConnected {
                peer, addr,
            });
            prop_assert!(commands.contains(&Command::KademliaBootstrap));
            prop_assert_eq!(state.relay, RelayState::Reserved { relay_peer: other_peer });
            let has_relay_cmd = commands.iter().any(|c| matches!(
                c,
                Command::RequestRelayReservation { .. }
            ));
            prop_assert!(!has_relay_cmd);
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
            state.phase = Phase::Discovering;
            let (state, _) = state.transition(Event::NatStatusChanged(from_status));
            prop_assert_eq!(state.nat_status, from_status);

            let (state, _) = state.transition(Event::NatStatusChanged(to_status));
            prop_assert_eq!(state.nat_status, to_status);
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
        fn no_bootstrap_peers_enters_participating(phase in arb_phase()) {
            let mut state = PeerState::new();
            state.phase = phase;

            let (new_state, _commands) = state.transition(Event::NoBootstrapPeers);
            match phase {
                Phase::Initializing => {
                    prop_assert_eq!(new_state.phase, Phase::Participating);
                }
                Phase::Discovering | Phase::Participating | Phase::ShuttingDown => {
                    prop_assert_eq!(new_state.phase, phase);
                }
            }
        }

        #[test]
        fn participating_always_requests_relay(
            peer in arb_peer_id(),
            addr in arb_multiaddr(),
        ) {
            let state = PeerState::new();
            let (state, _) = state.transition(Event::BootstrapConnected {
                peer, addr,
            });
            let (state, _commands) = state.transition(Event::KademliaBootstrapOk);
            prop_assert_eq!(state.phase, Phase::Participating);
            prop_assert_eq!(state.relay, RelayState::Requesting { relay_peer: peer });
        }

        #[test]
        fn private_nat_is_noop_for_relay(
            peer in arb_peer_id(),
            addr in arb_multiaddr(),
        ) {
            let state = PeerState::new();
            let (state, _) = state.transition(Event::BootstrapConnected {
                peer, addr,
            });
            let (state, _) = state.transition(Event::KademliaBootstrapOk);
            let relay_before = state.relay.clone();

            let (state, commands) = state.transition(Event::NatStatusChanged(NatStatus::Private));
            prop_assert_eq!(state.relay, relay_before);
            let has_relay_cmd = commands.iter().any(|c| matches!(
                c,
                Command::RequestRelayReservation { .. }
            ));
            prop_assert!(!has_relay_cmd);
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
        fn relay_accepted_transitions_to_reserved(relay_peer in arb_peer_id()) {
            let mut state = PeerState::new();
            state.relay = RelayState::Requesting { relay_peer };

            let (new_state, _) = state.transition(Event::RelayReservationAccepted { relay_peer });
            prop_assert_eq!(new_state.relay, RelayState::Reserved { relay_peer });
        }

        #[test]
        fn relay_failed_returns_to_idle(
            relay_peer in arb_peer_id(),
            reason in "[a-z]{1,20}",
        ) {
            let mut state = PeerState::new();
            state.phase = Phase::Discovering;
            state.relay = RelayState::Requesting { relay_peer };

            let (new_state, _) = state.transition(Event::RelayReservationFailed {
                relay_peer,
                reason,
            });
            prop_assert_eq!(new_state.relay, RelayState::Idle);
        }

        #[test]
        fn public_nat_clears_relay(relay_peer in arb_peer_id()) {
            let mut state = PeerState::new();
            state.phase = Phase::Participating;
            state.relay = RelayState::Reserved { relay_peer };
            state.kad_bootstrapped = true;

            let (new_state, _) = state.transition(Event::NatStatusChanged(NatStatus::Public));
            prop_assert_eq!(new_state.relay, RelayState::Idle);
        }

        #[test]
        fn peer_identified_adds_all_addresses(
            peer in arb_peer_id(),
            addrs in proptest::collection::vec(arb_multiaddr(), 1..5),
        ) {
            let state = PeerState::new();
            let (new_state, commands) = state.transition(Event::PeerIdentified {
                peer,
                listen_addrs: addrs.clone(),
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
        fn connection_lost_removes_peer_and_regresses(
            peer in arb_peer_id(),
            addr in arb_multiaddr(),
        ) {
            let mut state = PeerState::new();
            state.phase = Phase::Participating;
            state.kad_bootstrapped = true;
            state.bootstrap_peers.insert(peer);
            state.known_peers.entry(peer).or_default().insert(addr);

            let (new_state, commands) = state.transition(Event::ConnectionLost {
                peer,
                remaining_connections: 0,
            });

            prop_assert!(!new_state.known_peers.contains_key(&peer));
            prop_assert_eq!(new_state.phase, Phase::Discovering);
            prop_assert!(!new_state.kad_bootstrapped);
            prop_assert!(commands.contains(&Command::KademliaBootstrap));
        }

        #[test]
        fn connection_lost_standalone_stays_participating(
            peer in arb_peer_id(),
            addr in arb_multiaddr(),
        ) {
            let mut state = PeerState::new();
            state.phase = Phase::Participating;
            state.kad_bootstrapped = true;
            state.known_peers.entry(peer).or_default().insert(addr);

            let (new_state, commands) = state.transition(Event::ConnectionLost {
                peer,
                remaining_connections: 0,
            });

            prop_assert!(!new_state.known_peers.contains_key(&peer));
            prop_assert_eq!(new_state.phase, Phase::Participating);
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
            state.phase = Phase::Participating;
            state.known_peers.entry(peer).or_default().insert(addr);

            let (new_state, _) = state.transition(Event::ConnectionLost {
                peer,
                remaining_connections: remaining,
            });
            prop_assert!(new_state.known_peers.contains_key(&peer));
            prop_assert_eq!(new_state.phase, Phase::Participating);
        }

        #[test]
        fn listening_on_loopback_addr_no_external(
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
        fn listening_on_nonloopback_addr_emits_add_external(
            phase in arb_phase(),
            addr in arb_nonloopback_multiaddr(),
        ) {
            let mut state = PeerState::new();
            state.phase = phase;

            let (_, commands) = state.transition(Event::ListeningOn { addr: addr.clone() });
            let has_add_external = commands.iter().any(|c| matches!(
                c,
                Command::AddExternalAddress(a) if *a == addr
            ));
            prop_assert!(has_add_external);
        }

        #[test]
        fn listening_on_circuit_addr_emits_add_external(
            phase in arb_phase(),
            addr in arb_relay_circuit_multiaddr(),
        ) {
            let mut state = PeerState::new();
            state.phase = phase;

            let (_, commands) = state.transition(Event::ListeningOn { addr: addr.clone() });
            let has_add_external = commands.iter().any(|c| matches!(
                c,
                Command::AddExternalAddress(a) if *a == addr
            ));
            prop_assert!(has_add_external);
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
        fn nat_status_fromstr_roundtrip(
            status in prop_oneof![Just(NatStatus::Public), Just(NatStatus::Private)],
        ) {
            let s = status.to_string();
            let parsed: NatStatus = s.parse().expect("parse valid NatStatus string");
            prop_assert_eq!(parsed, status);
        }

        #[test]
        fn nat_status_fromstr_rejects_invalid(s in "[a-z]{1,10}") {
            prop_assume!(s != "private" && s != "public");
            let result: Result<NatStatus, _> = s.parse();
            prop_assert!(result.is_err());
        }

        #[test]
        fn kad_ok_with_empty_peers_stays_discovering(
            nat_status in arb_nat_status(),
        ) {
            let mut state = PeerState::new();
            state.phase = Phase::Discovering;
            state.nat_status = nat_status;

            let (state, _commands) = state.transition(Event::KademliaBootstrapOk);
            prop_assert_eq!(state.phase, Phase::Discovering);
            prop_assert!(state.kad_bootstrapped);
        }

        #[test]
        fn bootstrap_reconnect_in_discovering_enters_participating(
            peer in arb_peer_id(),
            addr in arb_multiaddr(),
        ) {
            let mut state = PeerState::new();
            state.phase = Phase::Discovering;
            state.kad_bootstrapped = true;
            state.bootstrap_peers.insert(peer);

            let (state, commands) = state.transition(Event::BootstrapConnected {
                peer, addr,
            });
            prop_assert_eq!(state.phase, Phase::Participating);
            prop_assert!(commands.contains(&Command::KademliaBootstrap));
        }

        #[test]
        fn relay_addr_prefers_non_loopback(
            peer in arb_peer_id(),
            external_addr in arb_nonloopback_multiaddr(),
            port in 1024u16..65535u16,
        ) {
            let loopback_addr: Multiaddr = format!("/ip4/127.0.0.1/tcp/{port}")
                .parse()
                .expect("loopback multiaddr is always valid");

            let mut state = PeerState::new();
            state.phase = Phase::Discovering;
            state.bootstrap_peers.insert(peer);
            let addrs = state.known_peers.entry(peer).or_default();
            addrs.insert(loopback_addr);
            addrs.insert(external_addr.clone());

            let (_, commands) = state.transition(Event::KademliaBootstrapOk);

            let relay_addr = commands.iter().find_map(|c| match c {
                Command::RequestRelayReservation { relay_addr, .. } => Some(relay_addr),
                _ => None,
            });
            if let Some(addr) = relay_addr {
                prop_assert!(!is_loopback_addr(addr));
            }
        }

        #[test]
        fn connection_lost_resets_relay_for_relay_peer(
            peer in arb_peer_id(),
            addr in arb_multiaddr(),
            other_peer in arb_peer_id(),
            other_addr in arb_multiaddr(),
        ) {
            prop_assume!(peer != other_peer);

            let mut state = PeerState::new();
            state.phase = Phase::Participating;
            state.relay = RelayState::Reserved { relay_peer: peer };
            state.known_peers.entry(peer).or_default().insert(addr);
            state.known_peers.entry(other_peer).or_default().insert(other_addr);

            let (state, _) = state.transition(Event::ConnectionLost {
                peer,
                remaining_connections: 0,
            });

            prop_assert_eq!(state.relay, RelayState::Idle);
            prop_assert_eq!(state.phase, Phase::Participating);
        }
    }
}
