use libp2p::{Multiaddr, PeerId};

use crate::{
    specs::ServiceAddr,
    types::{KademliaKey, ServiceName},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    // ─── Network (from PeerState) ───────────────────────────────────────────
    Dial(Multiaddr),
    Listen(Multiaddr),
    KademliaBootstrap,
    KademliaAddAddress {
        peer: PeerId,
        addr: Multiaddr,
    },
    RequestRelayReservation {
        relay_peer: PeerId,
        relay_addr: Multiaddr,
    },
    AddExternalAddress(Multiaddr),
    Shutdown,

    // ─── Tunnel (from TunnelState) ──────────────────────────────────────────
    PublishServices,
    DhtLookupPeer {
        peer: PeerId,
    },
    DhtGetProviders {
        service_name: ServiceName,
        key: KademliaKey,
    },
    DialPeer {
        peer: PeerId,
    },
    SpawnTunnel {
        peer: PeerId,
        service: ServiceName,
        bind: ServiceAddr,
    },
}
