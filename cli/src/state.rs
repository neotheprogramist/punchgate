use std::collections::{HashMap, HashSet};

use libp2p::{Multiaddr, PeerId};

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
    use super::*;

    fn test_peer() -> PeerId {
        PeerId::random()
    }

    fn test_addr() -> Multiaddr {
        "/ip4/127.0.0.1/tcp/9000"
            .parse()
            .expect("parse test multiaddr")
    }

    #[test]
    fn starts_in_initializing() {
        let state = PeerState::new();
        assert_eq!(state.phase, Phase::Initializing);
        assert_eq!(state.nat_status, NatStatus::Unknown);
        assert_eq!(state.relay, RelayState::Idle);
        assert!(!state.kad_bootstrapped);
        assert!(!state.autonat_resolved);
    }

    #[test]
    fn bootstrap_connected_transitions_to_discovering() {
        let state = PeerState::new();
        let peer = test_peer();
        let addr = test_addr();

        let (state, commands) = state.transition(Event::BootstrapConnected {
            peer,
            addr: addr.clone(),
        });

        assert_eq!(state.phase, Phase::Discovering);
        assert!(commands.contains(&Command::KademliaBootstrap));
        assert!(commands.contains(&Command::KademliaAddAddress { peer, addr }));
    }

    #[test]
    fn second_bootstrap_does_not_re_enter_discovering() {
        let state = PeerState::new();
        let peer1 = test_peer();
        let peer2 = test_peer();

        let (state, _) = state.transition(Event::BootstrapConnected {
            peer: peer1,
            addr: test_addr(),
        });
        assert_eq!(state.phase, Phase::Discovering);

        let (state, commands) = state.transition(Event::BootstrapConnected {
            peer: peer2,
            addr: test_addr(),
        });
        assert_eq!(state.phase, Phase::Discovering);
        // Should NOT re-issue KademliaBootstrap
        assert!(!commands.contains(&Command::KademliaBootstrap));
    }

    #[test]
    fn kad_plus_autonat_transitions_to_participating() {
        let state = PeerState::new();
        let (state, _) = state.transition(Event::BootstrapConnected {
            peer: test_peer(),
            addr: test_addr(),
        });

        // Kademlia first, then AutoNAT
        let (state, _) = state.transition(Event::KademliaBootstrapOk);
        assert_eq!(state.phase, Phase::Discovering);

        let (state, commands) = state.transition(Event::NatStatusChanged(NatStatus::Public));
        assert_eq!(state.phase, Phase::Participating);
        assert!(commands.contains(&Command::PublishServices));
    }

    #[test]
    fn autonat_plus_kad_transitions_to_participating() {
        // Bisimulation: same result regardless of event order
        let state = PeerState::new();
        let (state, _) = state.transition(Event::BootstrapConnected {
            peer: test_peer(),
            addr: test_addr(),
        });

        // AutoNAT first, then Kademlia
        let (state, _) = state.transition(Event::NatStatusChanged(NatStatus::Public));
        assert_eq!(state.phase, Phase::Discovering);

        let (state, commands) = state.transition(Event::KademliaBootstrapOk);
        assert_eq!(state.phase, Phase::Participating);
        assert!(commands.contains(&Command::PublishServices));
    }

    #[test]
    fn discovery_timeout_forces_participating() {
        let state = PeerState::new();
        let (state, _) = state.transition(Event::BootstrapConnected {
            peer: test_peer(),
            addr: test_addr(),
        });
        assert_eq!(state.phase, Phase::Discovering);

        let (state, commands) = state.transition(Event::DiscoveryTimeout);
        assert_eq!(state.phase, Phase::Participating);
        assert!(commands.contains(&Command::PublishServices));
    }

    #[test]
    fn shutdown_from_any_phase() {
        for initial_phase in [
            Phase::Initializing,
            Phase::Discovering,
            Phase::Participating,
        ] {
            let mut state = PeerState::new();
            state.phase = initial_phase;

            let (state, commands) = state.transition(Event::ShutdownRequested);
            assert_eq!(
                state.phase,
                Phase::ShuttingDown,
                "shutdown from {initial_phase:?} should enter ShuttingDown"
            );
            assert!(commands.contains(&Command::Shutdown));
        }
    }

    #[test]
    fn nat_transitions_are_symmetric() {
        // Public → Private and Private → Public are both valid
        let mut state = PeerState::new();
        state.phase = Phase::Discovering;

        let (state, _) = state.transition(Event::NatStatusChanged(NatStatus::Public));
        assert_eq!(state.nat_status, NatStatus::Public);

        let (state, _) = state.transition(Event::NatStatusChanged(NatStatus::Private));
        assert_eq!(state.nat_status, NatStatus::Private);
    }

    #[test]
    fn relay_reservation_lifecycle() {
        let bootstrap_peer = test_peer();
        let bootstrap_addr = test_addr();
        let mut state = PeerState::new();
        state.phase = Phase::Discovering;
        state
            .known_peers
            .entry(bootstrap_peer)
            .or_default()
            .insert(bootstrap_addr);
        state.bootstrap_peers.insert(bootstrap_peer);

        // NAT becomes Private → should request relay
        let (state, commands) = state.transition(Event::NatStatusChanged(NatStatus::Private));
        assert_eq!(
            state.relay,
            RelayState::Requesting {
                relay_peer: bootstrap_peer
            }
        );
        assert!(commands.iter().any(|c| matches!(
            c,
            Command::RequestRelayReservation { relay_peer, .. }
            if *relay_peer == bootstrap_peer
        )));

        // Reservation accepted
        let (state, _) = state.transition(Event::RelayReservationAccepted {
            relay_peer: bootstrap_peer,
        });
        assert_eq!(
            state.relay,
            RelayState::Reserved {
                relay_peer: bootstrap_peer
            }
        );
    }

    #[test]
    fn public_nat_clears_relay() {
        let relay_peer = test_peer();
        let mut state = PeerState::new();
        state.phase = Phase::Participating;
        state.relay = RelayState::Reserved { relay_peer };
        state.autonat_resolved = true;
        state.kad_bootstrapped = true;

        let (state, _) = state.transition(Event::NatStatusChanged(NatStatus::Public));
        assert_eq!(state.relay, RelayState::Idle);
    }

    #[test]
    fn mdns_does_not_affect_phase_or_nat() {
        let state = PeerState::new();
        let peer = test_peer();
        let addr = test_addr();

        let (state, commands) = state.transition(Event::MdnsDiscovered {
            peers: vec![(peer, addr.clone())],
        });

        // Phase and NAT untouched (orthogonality)
        assert_eq!(state.phase, Phase::Initializing);
        assert_eq!(state.nat_status, NatStatus::Unknown);

        // But peer is tracked and KademliaAddAddress emitted
        assert!(state.known_peers.contains_key(&peer));
        assert!(commands.contains(&Command::KademliaAddAddress { peer, addr }));
    }

    #[test]
    fn connection_lost_regresses_to_discovering() {
        let peer = test_peer();
        let addr = test_addr();
        let mut state = PeerState::new();
        state.phase = Phase::Participating;
        state.kad_bootstrapped = true;
        state.autonat_resolved = true;
        state.known_peers.entry(peer).or_default().insert(addr);

        let (state, commands) = state.transition(Event::ConnectionLost {
            peer,
            remaining_connections: 0,
        });

        assert_eq!(state.phase, Phase::Discovering);
        assert!(!state.kad_bootstrapped);
        assert!(commands.contains(&Command::KademliaBootstrap));
    }

    #[test]
    fn relay_reservation_failed_returns_to_idle() {
        let relay_peer = test_peer();
        let mut state = PeerState::new();
        state.phase = Phase::Discovering;
        state.relay = RelayState::Requesting { relay_peer };

        let (state, _) = state.transition(Event::RelayReservationFailed {
            relay_peer,
            reason: "timeout".into(),
        });

        assert_eq!(state.relay, RelayState::Idle);
    }

    #[test]
    fn peer_identified_adds_addresses() {
        let state = PeerState::new();
        let peer = test_peer();
        let addr1: Multiaddr = "/ip4/10.0.0.1/tcp/1000"
            .parse()
            .expect("parse test multiaddr 1");
        let addr2: Multiaddr = "/ip4/10.0.0.1/tcp/2000"
            .parse()
            .expect("parse test multiaddr 2");

        let (state, commands) = state.transition(Event::PeerIdentified {
            peer,
            listen_addrs: vec![addr1.clone(), addr2.clone()],
        });

        let addrs = state
            .known_peers
            .get(&peer)
            .expect("peer should be in known_peers");
        assert!(addrs.contains(&addr1));
        assert!(addrs.contains(&addr2));
        assert_eq!(
            commands
                .iter()
                .filter(|c| matches!(c, Command::KademliaAddAddress { .. }))
                .count(),
            2
        );
    }

    #[test]
    fn mdns_expired_removes_addresses() {
        let peer = test_peer();
        let addr1: Multiaddr = "/ip4/10.0.0.1/tcp/1000"
            .parse()
            .expect("parse test multiaddr 1");
        let addr2: Multiaddr = "/ip4/10.0.0.1/tcp/2000"
            .parse()
            .expect("parse test multiaddr 2");

        let mut state = PeerState::new();
        state
            .known_peers
            .entry(peer)
            .or_default()
            .insert(addr1.clone());
        state
            .known_peers
            .entry(peer)
            .or_default()
            .insert(addr2.clone());

        // Expire one address — peer still known
        let (state, _) = state.transition(Event::MdnsExpired {
            peers: vec![(peer, addr1)],
        });
        assert!(state.known_peers.contains_key(&peer));

        // Expire second address — peer removed entirely
        let (state, _) = state.transition(Event::MdnsExpired {
            peers: vec![(peer, addr2)],
        });
        assert!(!state.known_peers.contains_key(&peer));
    }
}
