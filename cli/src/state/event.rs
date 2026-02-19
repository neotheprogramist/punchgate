use libp2p::{Multiaddr, PeerId};

use super::peer::{NatStatus, Phase};
use crate::types::ServiceName;

#[derive(Debug, Clone, PartialEq, Eq, strum::Display)]
pub enum Event {
    // ─── Network lifecycle (PeerState) ──────────────────────────────────────
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
        observed_addr: Multiaddr,
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
    ConnectionLost {
        peer: PeerId,
        remaining_connections: u32,
    },
    NoBootstrapPeers,
    ExternalAddrConfirmed {
        addr: Multiaddr,
    },
    ExternalAddrExpired {
        addr: Multiaddr,
    },

    // ─── Tunnel lifecycle (TunnelState) ─────────────────────────────────────
    DhtPeerLookupComplete {
        peer: PeerId,
        connected: bool,
    },
    DhtServiceResolved {
        service_name: ServiceName,
        provider: PeerId,
        connected: bool,
    },
    DhtServiceFailed {
        service_name: ServiceName,
        reason: String,
    },
    TunnelPeerConnected {
        peer: PeerId,
        relayed: bool,
    },
    HolePunchSucceeded {
        remote_peer: PeerId,
    },
    HolePunchFailed {
        remote_peer: PeerId,
        reason: String,
    },
    HolePunchTimeout {
        peer: PeerId,
    },
    // ─── Bridge (synthesized by AppState) ───────────────────────────────────
    PhaseChanged {
        old: Phase,
        new: Phase,
    },
}
