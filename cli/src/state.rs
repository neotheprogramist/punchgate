use std::collections::{HashMap, HashSet};

use libp2p::{Multiaddr, PeerId, multiaddr::Protocol};

// ─── Command (output alphabet) ─────────────────────────────────────────────

/// Commands are the output alphabet of the Mealy machine.
/// They describe side effects without executing them — the event loop
/// interprets them against the actual swarm.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Dial(Multiaddr),
    Listen(Multiaddr),
    KademliaBootstrap,
    KademliaAddAddress {
        peer: PeerId,
        addr: Multiaddr,
    },
    KademliaStartProviding {
        key: Vec<u8>,
    },
    KademliaPutRecord {
        key: Vec<u8>,
        value: Vec<u8>,
    },
    RequestRelayReservation {
        relay_peer: PeerId,
        relay_addr: Multiaddr,
    },
    AddExternalAddress(Multiaddr),
    PublishServices,
    Log(String),
    Shutdown,
}

// ─── Event (input alphabet) ─────────────────────────────────────────────────

/// Events are the input alphabet — translated from raw `SwarmEvent`s
/// by the event loop before being fed to the state machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    ListeningOn {
        addr: Multiaddr,
    },
    BootstrapConnected {
        peer: PeerId,
        addr: Multiaddr,
    },
    ShutdownRequested,
    KademliaBootstrapOk,
    KademliaBootstrapFailed {
        reason: String,
    },
    MdnsDiscovered {
        peers: Vec<(PeerId, Multiaddr)>,
    },
    MdnsExpired {
        peers: Vec<(PeerId, Multiaddr)>,
    },
    PeerIdentified {
        peer: PeerId,
        listen_addrs: Vec<Multiaddr>,
    },
    NatStatusChanged(NatStatus),
    DiscoveryTimeout,
    RelayReservationAccepted {
        relay_peer: PeerId,
    },
    RelayReservationFailed {
        relay_peer: PeerId,
        reason: String,
    },
    HolePunchSucceeded {
        remote_peer: PeerId,
    },
    HolePunchFailed {
        remote_peer: PeerId,
        reason: String,
    },
    ConnectionLost {
        peer: PeerId,
        remaining_connections: u32,
    },
}

// ─── State types (product of orthogonal coproducts) ─────────────────────────

/// Phase of the peer's lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Phase {
    /// Just started, waiting for bootstrap connection.
    Initializing,
    /// Connected to bootstrap, running Kademlia + AutoNAT discovery.
    Discovering,
    /// Fully operational — both Kad and AutoNAT resolved (or timed out).
    Participating,
    /// Graceful shutdown in progress.
    ShuttingDown,
}

/// NAT reachability status, mirroring libp2p's AutoNAT result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NatStatus {
    Unknown,
    Public,
    Private,
}

/// State of relay circuit reservation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayState {
    Idle,
    Requesting { relay_peer: PeerId },
    Reserved { relay_peer: PeerId },
}

/// The full peer state — a product of orthogonal coproducts.
///
/// Each field represents an independent dimension of the state space.
/// The transition function is a Mealy machine coalgebra:
///   `(PeerState, Event) → (PeerState, Vec<Command>)`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerState {
    pub phase: Phase,
    pub nat_status: NatStatus,
    pub relay: RelayState,
    pub known_peers: HashMap<PeerId, HashSet<Multiaddr>>,
    pub bootstrap_peers: HashSet<PeerId>,
    pub kad_bootstrapped: bool,
    pub autonat_resolved: bool,
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
            autonat_resolved: false,
        }
    }

    /// Pure transition function: the heart of the coalgebra.
    ///
    /// Given the current state and an input event, produces a new state
    /// and a list of commands (side effects to execute).
    pub fn transition(mut self, event: Event) -> (Self, Vec<Command>) {
        let mut commands = Vec::new();

        match event {
            Event::ListeningOn { addr } => {
                commands.push(Command::Log(format!("listening on {addr}")));
                if addr.iter().any(|p| matches!(p, Protocol::P2pCircuit)) {
                    commands.push(Command::AddExternalAddress(addr));
                }
            }

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

                if self.phase == Phase::Initializing {
                    self.phase = Phase::Discovering;
                    commands.push(Command::KademliaBootstrap);
                    commands.push(Command::Log(format!(
                        "bootstrap peer {peer} connected, entering discovery"
                    )));
                }
            }

            Event::KademliaBootstrapOk => {
                self.kad_bootstrapped = true;
                commands.push(Command::Log("kademlia bootstrap complete".into()));
                let (s, cmds) = self.maybe_enter_participating();
                self = s;
                commands.extend(cmds);
            }

            Event::KademliaBootstrapFailed { reason } => {
                commands.push(Command::Log(format!("kademlia bootstrap failed: {reason}")));
            }

            Event::NatStatusChanged(status) => {
                self.autonat_resolved = true;
                let old = self.nat_status;
                self.nat_status = status;

                commands.push(Command::Log(format!(
                    "NAT status changed: {old:?} → {status:?}"
                )));

                match (status, &self.relay) {
                    (NatStatus::Private, RelayState::Idle) => {
                        // Behind NAT and no relay — find one from known peers
                        if let Some((&relay_peer, addrs)) = self
                            .known_peers
                            .iter()
                            .find(|(p, _)| self.bootstrap_peers.contains(p))
                            && let Some(addr) = addrs.iter().next()
                        {
                            self.relay = RelayState::Requesting { relay_peer };
                            commands.push(Command::RequestRelayReservation {
                                relay_peer,
                                relay_addr: addr.clone(),
                            });
                        }
                    }
                    (NatStatus::Public, RelayState::Reserved { .. }) => {
                        // We're publicly reachable now — drop relay
                        self.relay = RelayState::Idle;
                        commands.push(Command::Log(
                            "publicly reachable, releasing relay reservation".into(),
                        ));
                    }
                    (
                        NatStatus::Unknown,
                        RelayState::Idle
                        | RelayState::Requesting { .. }
                        | RelayState::Reserved { .. },
                    ) => {}
                    (NatStatus::Public, RelayState::Idle | RelayState::Requesting { .. }) => {}
                    (
                        NatStatus::Private,
                        RelayState::Requesting { .. } | RelayState::Reserved { .. },
                    ) => {}
                }

                let (s, cmds) = self.maybe_enter_participating();
                self = s;
                commands.extend(cmds);
            }

            Event::DiscoveryTimeout => {
                if self.phase == Phase::Discovering {
                    self.phase = Phase::Participating;
                    commands.push(Command::PublishServices);
                    commands.push(Command::Log(
                        "discovery timeout, entering participation".into(),
                    ));
                }
            }

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
                    if let Some(addrs) = self.known_peers.get_mut(&peer) {
                        addrs.remove(&addr);
                        if addrs.is_empty() {
                            self.known_peers.remove(&peer);
                        }
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
                commands.push(Command::PublishServices);
                commands.push(Command::Log(format!(
                    "relay reservation accepted via {relay_peer}"
                )));
            }

            Event::RelayReservationFailed { relay_peer, reason } => {
                if self.relay == (RelayState::Requesting { relay_peer }) {
                    self.relay = RelayState::Idle;
                }
                commands.push(Command::Log(format!(
                    "relay reservation with {relay_peer} failed: {reason}"
                )));
            }

            Event::HolePunchSucceeded { remote_peer } => {
                commands.push(Command::Log(format!(
                    "hole punch succeeded with {remote_peer}"
                )));
            }

            Event::HolePunchFailed {
                remote_peer,
                reason,
            } => {
                commands.push(Command::Log(format!(
                    "hole punch with {remote_peer} failed: {reason}"
                )));
            }

            Event::ConnectionLost {
                peer,
                remaining_connections,
            } => {
                if remaining_connections == 0 {
                    self.known_peers.remove(&peer);
                    commands.push(Command::Log(format!("peer {peer} fully disconnected")));

                    // If we've lost all non-bootstrap peers and are
                    // participating, regress to Discovering
                    if self.phase == Phase::Participating && self.known_peers.is_empty() {
                        self.phase = Phase::Discovering;
                        self.kad_bootstrapped = false;
                        commands.push(Command::KademliaBootstrap);
                        commands.push(Command::Log("all peers lost, re-entering discovery".into()));
                    }
                }
            }

            Event::ShutdownRequested => {
                self.phase = Phase::ShuttingDown;
                commands.push(Command::Shutdown);
            }
        }

        (self, commands)
    }

    /// Check if we can transition from Discovering → Participating.
    ///
    /// This requires BOTH Kademlia bootstrap AND AutoNAT to have resolved.
    /// The function is idempotent — calling it multiple times won't
    /// re-enter Participating.
    fn maybe_enter_participating(mut self) -> (Self, Vec<Command>) {
        let mut commands = Vec::new();
        if self.phase == Phase::Discovering && self.kad_bootstrapped && self.autonat_resolved {
            self.phase = Phase::Participating;
            commands.push(Command::PublishServices);
            commands.push(Command::Log(
                "discovery complete, entering participation".into(),
            ));
        }
        (self, commands)
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    // ─── Generators ──────────────────────────────────────────────────────

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

    fn arb_phase() -> impl Strategy<Value = Phase> {
        prop_oneof![
            Just(Phase::Initializing),
            Just(Phase::Discovering),
            Just(Phase::Participating),
            Just(Phase::ShuttingDown),
        ]
    }

    fn arb_nat_status() -> impl Strategy<Value = NatStatus> {
        prop_oneof![
            Just(NatStatus::Unknown),
            Just(NatStatus::Public),
            Just(NatStatus::Private),
        ]
    }

    fn arb_relay_circuit_multiaddr() -> impl Strategy<Value = Multiaddr> {
        (arb_multiaddr(), arb_peer_id()).prop_map(|(base, relay_peer)| {
            let mut addr = base;
            addr.push(libp2p::multiaddr::Protocol::P2p(relay_peer));
            addr.push(libp2p::multiaddr::Protocol::P2pCircuit);
            addr
        })
    }

    // ─── Properties ──────────────────────────────────────────────────────

    #[test]
    fn initial_state_invariants() {
        let state = PeerState::new();
        assert_eq!(state.phase, Phase::Initializing);
        assert_eq!(state.nat_status, NatStatus::Unknown);
        assert_eq!(state.relay, RelayState::Idle);
        assert!(state.known_peers.is_empty());
        assert!(state.bootstrap_peers.is_empty());
        assert!(!state.kad_bootstrapped);
        assert!(!state.autonat_resolved);
    }

    proptest! {
        // Shutdown is an absorbing state — reachable from any phase
        #[test]
        fn shutdown_absorbs_any_phase(phase in arb_phase()) {
            let mut state = PeerState::new();
            state.phase = phase;
            let (new_state, commands) = state.transition(Event::ShutdownRequested);
            prop_assert_eq!(new_state.phase, Phase::ShuttingDown);
            prop_assert!(commands.contains(&Command::Shutdown));
        }

        // BootstrapConnected from Initializing always enters Discovering
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

        // Second BootstrapConnected doesn't re-issue KademliaBootstrap
        #[test]
        fn second_bootstrap_no_reenter(
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
            prop_assert!(!commands.contains(&Command::KademliaBootstrap));
        }

        // Bisimulation: Kad then AutoNAT ≡ AutoNAT then Kad for reaching Participating
        #[test]
        fn discovery_completion_is_commutative(
            peer in arb_peer_id(),
            addr in arb_multiaddr(),
            nat_status in prop_oneof![Just(NatStatus::Public), Just(NatStatus::Private)],
        ) {
            // Path A: Kad first, then AutoNAT
            let state_a = PeerState::new();
            let (state_a, _) = state_a.transition(Event::BootstrapConnected {
                peer, addr: addr.clone(),
            });
            let (state_a, _) = state_a.transition(Event::KademliaBootstrapOk);
            let (state_a, _) = state_a.transition(Event::NatStatusChanged(nat_status));

            // Path B: AutoNAT first, then Kad
            let state_b = PeerState::new();
            let (state_b, _) = state_b.transition(Event::BootstrapConnected {
                peer, addr,
            });
            let (state_b, _) = state_b.transition(Event::NatStatusChanged(nat_status));
            let (state_b, _) = state_b.transition(Event::KademliaBootstrapOk);

            // Both paths must reach Participating with identical discovery flags
            prop_assert_eq!(state_a.phase, Phase::Participating);
            prop_assert_eq!(state_b.phase, Phase::Participating);
            prop_assert_eq!(state_a.kad_bootstrapped, state_b.kad_bootstrapped);
            prop_assert_eq!(state_a.autonat_resolved, state_b.autonat_resolved);
        }

        // DiscoveryTimeout from Discovering forces Participating
        #[test]
        fn discovery_timeout_forces_participating(
            peer in arb_peer_id(),
            addr in arb_multiaddr(),
        ) {
            let state = PeerState::new();
            let (state, _) = state.transition(Event::BootstrapConnected { peer, addr });
            prop_assert_eq!(state.phase, Phase::Discovering);

            let (state, commands) = state.transition(Event::DiscoveryTimeout);
            prop_assert_eq!(state.phase, Phase::Participating);
            prop_assert!(commands.contains(&Command::PublishServices));
        }

        // NatStatusChanged always updates the nat_status field and sets autonat_resolved
        #[test]
        fn nat_status_always_updates(
            phase in arb_phase(),
            new_status in arb_nat_status(),
        ) {
            let mut state = PeerState::new();
            state.phase = phase;
            let (new_state, _) = state.transition(Event::NatStatusChanged(new_status));
            prop_assert_eq!(new_state.nat_status, new_status);
            prop_assert!(new_state.autonat_resolved);
        }

        // NAT transitions are symmetric — any status can go to any other
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

        // mDNS events are orthogonal — they never change phase or NAT status
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

        // mDNS expired removes only specified addresses; peer removed when empty
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

            if addr1 == addr2 {
                prop_assert!(!state.known_peers.contains_key(&peer));
            } else {
                prop_assert!(state.known_peers.contains_key(&peer));

                let (state, _) = state.transition(Event::MdnsExpired {
                    peers: vec![(peer, addr2)],
                });
                prop_assert!(!state.known_peers.contains_key(&peer));
            }
        }

        // Private NAT with a bootstrap peer in known_peers triggers relay request
        #[test]
        fn private_nat_requests_relay_from_bootstrap(
            bootstrap_peer in arb_peer_id(),
            bootstrap_addr in arb_multiaddr(),
        ) {
            let mut state = PeerState::new();
            state.phase = Phase::Discovering;
            state.known_peers.entry(bootstrap_peer).or_default().insert(bootstrap_addr);
            state.bootstrap_peers.insert(bootstrap_peer);

            let (new_state, commands) = state.transition(Event::NatStatusChanged(NatStatus::Private));
            prop_assert_eq!(
                new_state.relay,
                RelayState::Requesting { relay_peer: bootstrap_peer }
            );
            let has_relay_cmd = commands.iter().any(|c| matches!(
                c,
                Command::RequestRelayReservation { relay_peer, .. }
                if *relay_peer == bootstrap_peer
            ));
            prop_assert!(has_relay_cmd);
        }

        // Relay reservation accepted transitions to Reserved
        #[test]
        fn relay_accepted_transitions_to_reserved(relay_peer in arb_peer_id()) {
            let mut state = PeerState::new();
            state.relay = RelayState::Requesting { relay_peer };

            let (new_state, _) = state.transition(Event::RelayReservationAccepted { relay_peer });
            prop_assert_eq!(new_state.relay, RelayState::Reserved { relay_peer });
        }

        // Relay reservation failed from matching peer returns to Idle
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

        // Public NAT clears a reserved relay
        #[test]
        fn public_nat_clears_relay(relay_peer in arb_peer_id()) {
            let mut state = PeerState::new();
            state.phase = Phase::Participating;
            state.relay = RelayState::Reserved { relay_peer };
            state.autonat_resolved = true;
            state.kad_bootstrapped = true;

            let (new_state, _) = state.transition(Event::NatStatusChanged(NatStatus::Public));
            prop_assert_eq!(new_state.relay, RelayState::Idle);
        }

        // PeerIdentified adds all provided addresses and emits matching commands
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

        // ConnectionLost with 0 remaining removes peer and regresses when empty
        #[test]
        fn connection_lost_removes_peer_and_regresses(
            peer in arb_peer_id(),
            addr in arb_multiaddr(),
        ) {
            let mut state = PeerState::new();
            state.phase = Phase::Participating;
            state.kad_bootstrapped = true;
            state.autonat_resolved = true;
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

        // ConnectionLost with remaining > 0 preserves the peer
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

        // ListeningOn with a plain (non-circuit) addr emits only Log, no AddExternalAddress
        #[test]
        fn listening_on_plain_addr_no_external(
            phase in arb_phase(),
            addr in arb_multiaddr(),
        ) {
            let mut state = PeerState::new();
            state.phase = phase;
            let state_before = state.clone();

            let (new_state, commands) = state.transition(Event::ListeningOn { addr });
            prop_assert_eq!(new_state.phase, state_before.phase);
            prop_assert_eq!(new_state.nat_status, state_before.nat_status);
            prop_assert_eq!(new_state.relay, state_before.relay);
            prop_assert_eq!(new_state.known_peers, state_before.known_peers);
            prop_assert!(commands.iter().all(|c| matches!(c, Command::Log(_))));
            prop_assert!(!commands.iter().any(|c| matches!(c, Command::AddExternalAddress(_))));
        }

        // ListeningOn with a P2pCircuit addr emits AddExternalAddress
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

        // RelayReservationAccepted emits PublishServices to refresh DHT
        #[test]
        fn relay_accepted_emits_publish_services(relay_peer in arb_peer_id()) {
            let mut state = PeerState::new();
            state.relay = RelayState::Requesting { relay_peer };

            let (_, commands) = state.transition(Event::RelayReservationAccepted { relay_peer });
            prop_assert!(commands.contains(&Command::PublishServices));
        }
    }
}
