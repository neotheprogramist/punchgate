use std::{
    collections::{HashMap, HashSet},
    fmt,
    str::FromStr,
};

use libp2p::{Multiaddr, PeerId, multiaddr::Protocol};
use thiserror::Error;

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
    KademliaGetProviders {
        key: Vec<u8>,
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
    ServiceProvidersFound {
        service_name: String,
        providers: Vec<PeerId>,
    },
    ServiceLookupFailed {
        service_name: String,
        reason: String,
    },
    NoBootstrapPeers,
    ExternalAddrConfirmed {
        addr: Multiaddr,
    },
    ExternalAddrExpired {
        addr: Multiaddr,
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

impl fmt::Display for NatStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NatStatus::Unknown => write!(f, "unknown"),
            NatStatus::Public => write!(f, "public"),
            NatStatus::Private => write!(f, "private"),
        }
    }
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

    /// Pure transition function: the heart of the coalgebra.
    ///
    /// Given the current state and an input event, produces a new state
    /// and a list of commands (side effects to execute).
    pub fn transition(mut self, event: Event) -> (Self, Vec<Command>) {
        let mut commands = Vec::new();

        match event {
            Event::ListeningOn { addr } => {
                commands.push(Command::Log(format!("listening on {addr}")));
                match is_loopback_addr(&addr) {
                    true => {}
                    false => commands.push(Command::AddExternalAddress(addr)),
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

                match self.phase {
                    Phase::Initializing => {
                        self.phase = Phase::Discovering;
                        commands.push(Command::KademliaBootstrap);
                        commands.push(Command::Log(format!(
                            "bootstrap peer {peer} connected, entering discovery"
                        )));
                    }
                    Phase::Discovering => {
                        commands.push(Command::KademliaBootstrap);
                        commands.push(Command::Log(format!(
                            "bootstrap peer {peer} reconnected, restarting discovery"
                        )));
                        let (s, cmds) = self.maybe_enter_participating();
                        self = s;
                        commands.extend(cmds);
                    }
                    Phase::Participating => {
                        commands.push(Command::KademliaBootstrap);
                        commands.push(Command::Log(format!(
                            "bootstrap peer {peer} reconnected during participation, refreshing"
                        )));
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
                commands.push(Command::Log("kademlia bootstrap complete".into()));
                let (s, cmds) = self.maybe_enter_participating();
                self = s;
                commands.extend(cmds);
            }

            Event::KademliaBootstrapFailed { reason } => {
                commands.push(Command::Log(format!("kademlia bootstrap failed: {reason}")));
            }

            Event::NatStatusChanged(status) => {
                let old = self.nat_status;
                self.nat_status = status;

                commands.push(Command::Log(format!(
                    "NAT status changed: {old:?} → {status:?}"
                )));

                match (status, &self.relay) {
                    (NatStatus::Public, RelayState::Reserved { .. }) => {
                        self.relay = RelayState::Idle;
                        commands.push(Command::Log(
                            "publicly reachable, releasing relay reservation".into(),
                        ));
                    }
                    (NatStatus::Public, RelayState::Idle | RelayState::Requesting { .. })
                    | (NatStatus::Private | NatStatus::Unknown, _) => {}
                }
            }

            Event::DiscoveryTimeout => match (self.phase, self.known_peers.is_empty()) {
                (Phase::Discovering, false) => {
                    self.phase = Phase::Participating;
                    commands.push(Command::PublishServices);
                    commands.push(Command::Log(
                        "discovery timeout, entering participation".into(),
                    ));
                }
                (Phase::Discovering, true) => {
                    commands.push(Command::Log(
                        "discovery timeout with no known peers, staying in discovery".into(),
                    ));
                }
                (Phase::Initializing | Phase::Participating | Phase::ShuttingDown, _) => {}
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
                commands.push(Command::PublishServices);
                commands.push(Command::Log(format!(
                    "relay reservation accepted via {relay_peer}"
                )));
            }

            Event::RelayReservationFailed { relay_peer, reason } => {
                match &self.relay {
                    RelayState::Requesting { relay_peer: rp } if *rp == relay_peer => {
                        self.relay = RelayState::Idle;
                    }
                    RelayState::Requesting { .. }
                    | RelayState::Idle
                    | RelayState::Reserved { .. } => {}
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

                    match &self.relay {
                        RelayState::Requesting { relay_peer }
                        | RelayState::Reserved { relay_peer }
                            if *relay_peer == peer =>
                        {
                            self.relay = RelayState::Idle;
                            commands.push(Command::Log(
                                "relay peer disconnected, resetting relay state".into(),
                            ));
                        }
                        RelayState::Requesting { .. }
                        | RelayState::Reserved { .. }
                        | RelayState::Idle => {}
                    }

                    match (self.phase, self.known_peers.is_empty()) {
                        (Phase::Participating, true) => {
                            self.phase = Phase::Discovering;
                            self.kad_bootstrapped = false;
                            commands.push(Command::KademliaBootstrap);
                            commands
                                .push(Command::Log("all peers lost, re-entering discovery".into()));
                        }
                        (Phase::Participating, false)
                        | (Phase::Initializing, _)
                        | (Phase::Discovering, _)
                        | (Phase::ShuttingDown, _) => {}
                    }
                }
            }

            Event::ServiceProvidersFound {
                service_name,
                providers,
            } => {
                commands.push(Command::Log(format!(
                    "found {} provider(s) for service '{service_name}'",
                    providers.len()
                )));
            }

            Event::ServiceLookupFailed {
                service_name,
                reason,
            } => {
                commands.push(Command::Log(format!(
                    "service lookup failed for '{service_name}': {reason}"
                )));
            }

            Event::NoBootstrapPeers => match self.phase {
                Phase::Initializing => {
                    self.phase = Phase::Participating;
                    commands.push(Command::PublishServices);
                    commands.push(Command::Log(
                        "no bootstrap peers configured, entering participation directly".into(),
                    ));
                }
                Phase::Discovering | Phase::Participating | Phase::ShuttingDown => {}
            },

            Event::ExternalAddrConfirmed { addr } => {
                self.external_addrs.insert(addr.clone());
                commands.push(Command::Log(format!("external address confirmed: {addr}")));
            }

            Event::ExternalAddrExpired { addr } => {
                self.external_addrs.remove(&addr);
                commands.push(Command::Log(format!("external address expired: {addr}")));
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
    /// Gate: `phase == Discovering && kad_bootstrapped`.
    /// Unconditionally requests relay reservation from the nearest bootstrap peer.
    fn maybe_enter_participating(mut self) -> (Self, Vec<Command>) {
        let mut commands = Vec::new();
        match (
            self.phase,
            self.kad_bootstrapped,
            self.known_peers.is_empty(),
        ) {
            (Phase::Discovering, true, false) => {
                self.phase = Phase::Participating;
                commands.push(Command::PublishServices);
                commands.push(Command::Log(
                    "discovery complete, entering participation".into(),
                ));

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

    fn arb_nonloopback_multiaddr() -> impl Strategy<Value = Multiaddr> {
        (
            (
                1u8..=126,
                0u8..=255,
                0u8..=255,
                1u8..=254,
                1024u16..65535u16,
            ),
            (
                128u8..=254,
                0u8..=255,
                0u8..=255,
                1u8..=254,
                1024u16..65535u16,
            ),
        )
            .prop_flat_map(|(lo, hi)| prop_oneof![Just(lo), Just(hi)])
            .prop_map(|(a, b, c, d, port)| {
                // Infallible: formatted string is always a valid multiaddr; a ∈ [1,126]∪[128,254] excludes 127
                format!("/ip4/{a}.{b}.{c}.{d}/tcp/{port}")
                    .parse()
                    .expect("generated non-loopback IP4/TCP multiaddr is always valid")
            })
    }

    fn arb_relay_circuit_multiaddr() -> impl Strategy<Value = Multiaddr> {
        (arb_nonloopback_multiaddr(), arb_peer_id()).prop_map(|(base, relay_peer)| {
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
        assert!(state.external_addrs.is_empty());
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

        // Second BootstrapConnected in Discovering re-issues KademliaBootstrap
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
            // Stays Discovering because kad_bootstrapped is still false
            prop_assert_eq!(state.phase, Phase::Discovering);
            prop_assert!(commands.contains(&Command::KademliaBootstrap));
        }

        // Kad completion alone triggers Participating (NAT is orthogonal)
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

            let (state, commands) = state.transition(Event::KademliaBootstrapOk);
            prop_assert_eq!(state.phase, Phase::Participating);
            prop_assert!(commands.contains(&Command::PublishServices));
        }

        // DiscoveryTimeout from Discovering enters Participating only with known peers
        #[test]
        fn discovery_timeout_enters_participating_with_peers(
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

        // DiscoveryTimeout with zero known peers stays in Discovering
        #[test]
        fn discovery_timeout_with_no_peers_stays_discovering(
            nat_status in arb_nat_status(),
        ) {
            let mut state = PeerState::new();
            state.phase = Phase::Discovering;
            state.nat_status = nat_status;

            let (state, commands) = state.transition(Event::DiscoveryTimeout);
            prop_assert_eq!(state.phase, Phase::Discovering);
            prop_assert!(!commands.contains(&Command::PublishServices));
        }

        // BootstrapConnected in Participating re-bootstraps Kademlia and requests relay
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

        // BootstrapConnected in Participating skips relay if already reserved
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

        // NatStatusChanged always updates the nat_status field
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

        // NoBootstrapPeers from Initializing enters Participating directly
        #[test]
        fn no_bootstrap_peers_enters_participating(phase in arb_phase()) {
            let mut state = PeerState::new();
            state.phase = phase;

            let (new_state, commands) = state.transition(Event::NoBootstrapPeers);
            match phase {
                Phase::Initializing => {
                    prop_assert_eq!(new_state.phase, Phase::Participating);
                    prop_assert!(commands.contains(&Command::PublishServices));
                }
                Phase::Discovering | Phase::Participating | Phase::ShuttingDown => {
                    prop_assert_eq!(new_state.phase, phase);
                }
            }
        }

        // Every Participating entry with a bootstrap peer emits RequestRelayReservation
        #[test]
        fn participating_always_requests_relay(
            peer in arb_peer_id(),
            addr in arb_multiaddr(),
        ) {
            let state = PeerState::new();
            let (state, _) = state.transition(Event::BootstrapConnected {
                peer, addr,
            });
            let (state, commands) = state.transition(Event::KademliaBootstrapOk);
            prop_assert_eq!(state.phase, Phase::Participating);
            prop_assert_eq!(state.relay, RelayState::Requesting { relay_peer: peer });
            let has_relay_cmd = commands.iter().any(|c| matches!(
                c,
                Command::RequestRelayReservation { relay_peer, .. }
                if *relay_peer == peer
            ));
            prop_assert!(has_relay_cmd);
        }

        // NatStatusChanged(Private) is a no-op for relay (relay already requested on Participating)
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

        // ExternalAddrConfirmed adds to set (idempotent)
        #[test]
        fn external_addr_confirmed_adds_to_set(
            addr in arb_multiaddr(),
        ) {
            let state = PeerState::new();
            let (state, _) = state.transition(Event::ExternalAddrConfirmed { addr: addr.clone() });
            prop_assert!(state.external_addrs.contains(&addr));

            // Idempotent
            let (state, _) = state.transition(Event::ExternalAddrConfirmed { addr: addr.clone() });
            prop_assert!(state.external_addrs.contains(&addr));
            prop_assert_eq!(state.external_addrs.len(), 1);
        }

        // ExternalAddrExpired removes from set
        #[test]
        fn external_addr_expired_removes_from_set(
            addr in arb_multiaddr(),
        ) {
            let mut state = PeerState::new();
            state.external_addrs.insert(addr.clone());

            let (state, _) = state.transition(Event::ExternalAddrExpired { addr: addr.clone() });
            prop_assert!(!state.external_addrs.contains(&addr));
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

        // ListeningOn with loopback addr emits only Log, no AddExternalAddress
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
            prop_assert!(commands.iter().all(|c| matches!(c, Command::Log(_))));
            prop_assert!(!commands.iter().any(|c| matches!(c, Command::AddExternalAddress(_))));
        }

        // ListeningOn with non-loopback addr emits AddExternalAddress
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

        // ServiceProvidersFound logs but preserves all state dimensions
        #[test]
        fn service_providers_found_logs_and_preserves_state(
            phase in arb_phase(),
            nat_status in arb_nat_status(),
            service_name in "[a-z]{1,10}",
            providers in proptest::collection::vec(arb_peer_id(), 0..5),
        ) {
            let mut state = PeerState::new();
            state.phase = phase;
            state.nat_status = nat_status;

            let (new_state, commands) = state.transition(Event::ServiceProvidersFound {
                service_name,
                providers,
            });

            prop_assert_eq!(new_state.phase, phase);
            prop_assert_eq!(new_state.nat_status, nat_status);
            let has_log = commands.iter().any(|c| matches!(c, Command::Log(_)));
            prop_assert!(has_log);
        }

        // ServiceLookupFailed logs but preserves all state dimensions
        #[test]
        fn service_lookup_failed_logs_and_preserves_state(
            phase in arb_phase(),
            nat_status in arb_nat_status(),
            service_name in "[a-z]{1,10}",
            reason in "[a-z ]{1,20}",
        ) {
            let mut state = PeerState::new();
            state.phase = phase;
            state.nat_status = nat_status;

            let (new_state, commands) = state.transition(Event::ServiceLookupFailed {
                service_name,
                reason,
            });

            prop_assert_eq!(new_state.phase, phase);
            prop_assert_eq!(new_state.nat_status, nat_status);
            let has_log = commands.iter().any(|c| matches!(c, Command::Log(_)));
            prop_assert!(has_log);
        }

        // RelayReservationAccepted emits PublishServices to refresh DHT
        #[test]
        fn relay_accepted_emits_publish_services(relay_peer in arb_peer_id()) {
            let mut state = PeerState::new();
            state.relay = RelayState::Requesting { relay_peer };

            let (_, commands) = state.transition(Event::RelayReservationAccepted { relay_peer });
            prop_assert!(commands.contains(&Command::PublishServices));
        }

        // NatStatus FromStr roundtrip for valid values
        #[test]
        fn nat_status_fromstr_roundtrip(
            status in prop_oneof![Just(NatStatus::Public), Just(NatStatus::Private)],
        ) {
            let s = status.to_string();
            let parsed: NatStatus = s.parse().expect("parse valid NatStatus string");
            prop_assert_eq!(parsed, status);
        }

        // NatStatus FromStr rejects invalid input
        #[test]
        fn nat_status_fromstr_rejects_invalid(s in "[a-z]{1,10}") {
            prop_assume!(s != "private" && s != "public");
            let result: Result<NatStatus, _> = s.parse();
            prop_assert!(result.is_err());
        }

        // KademliaBootstrapOk with empty known_peers stays in Discovering
        #[test]
        fn kad_ok_with_empty_peers_stays_discovering(
            nat_status in arb_nat_status(),
        ) {
            let mut state = PeerState::new();
            state.phase = Phase::Discovering;
            state.nat_status = nat_status;

            let (state, commands) = state.transition(Event::KademliaBootstrapOk);
            prop_assert_eq!(state.phase, Phase::Discovering);
            prop_assert!(state.kad_bootstrapped);
            prop_assert!(!commands.contains(&Command::PublishServices));
        }

        // Bootstrap reconnection in Discovering with prior kad success enters Participating
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
            prop_assert!(commands.contains(&Command::PublishServices));
            prop_assert!(commands.contains(&Command::KademliaBootstrap));
        }

        // Relay address selection prefers non-loopback over loopback
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

        // ConnectionLost resets relay state when relay peer disconnects
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
            // Does not regress to Discovering since other_peer remains
            prop_assert_eq!(state.phase, Phase::Participating);
        }
    }
}
