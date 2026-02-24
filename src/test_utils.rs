use std::net::SocketAddr;

use libp2p::{Multiaddr, PeerId, multiaddr::Protocol};
use proptest::prelude::*;

use crate::{
    state::{NatStatus, Phase},
    types::{HostAddr, ServiceName},
};

// ─── Peer / address generators ──────────────────────────────────────────────

pub fn arb_peer_id() -> impl Strategy<Value = PeerId> {
    any::<[u8; 32]>().prop_map(|bytes| {
        // Infallible: any 32 bytes is a valid ed25519 seed
        let secret = libp2p::identity::ed25519::SecretKey::try_from_bytes(bytes)
            .expect("any 32 bytes is a valid ed25519 seed");
        let ed_kp = libp2p::identity::ed25519::Keypair::from(secret);
        let keypair = libp2p::identity::Keypair::from(ed_kp);
        PeerId::from(keypair.public())
    })
}

pub fn arb_multiaddr() -> impl Strategy<Value = Multiaddr> {
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

pub fn arb_nonloopback_multiaddr() -> impl Strategy<Value = Multiaddr> {
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

pub fn arb_public_multiaddr() -> impl Strategy<Value = Multiaddr> {
    (
        1u8..=223,
        any::<u8>(),
        any::<u8>(),
        1u8..=254,
        1024u16..65535u16,
    )
        .prop_filter("must be public IP", |&(a, b, ..)| {
            let ip = std::net::Ipv4Addr::new(a, b, 0, 0);
            !ip.is_private() && !ip.is_loopback() && !ip.is_unspecified()
        })
        .prop_map(|(a, b, c, d, port)| {
            format!("/ip4/{a}.{b}.{c}.{d}/udp/{port}/quic-v1")
                .parse()
                .expect("generated public IP4/UDP/QUIC multiaddr is always valid")
        })
}

pub fn arb_private_multiaddr() -> impl Strategy<Value = Multiaddr> {
    prop_oneof![
        (any::<u8>(), any::<u8>(), any::<u8>(), 1024u16..65535u16)
            .prop_map(|(b, c, d, port)| format!("/ip4/10.{b}.{c}.{d}/udp/{port}/quic-v1")),
        (16u8..=31, any::<u8>(), any::<u8>(), 1024u16..65535u16)
            .prop_map(|(b, c, d, port)| format!("/ip4/172.{b}.{c}.{d}/udp/{port}/quic-v1")),
        (any::<u8>(), any::<u8>(), 1024u16..65535u16)
            .prop_map(|(c, d, port)| format!("/ip4/192.168.{c}.{d}/udp/{port}/quic-v1")),
    ]
    .prop_map(|s| {
        s.parse()
            .expect("generated private IP4/UDP/QUIC multiaddr is always valid")
    })
}

pub fn arb_relay_circuit_multiaddr() -> impl Strategy<Value = Multiaddr> {
    (arb_nonloopback_multiaddr(), arb_peer_id()).prop_map(|(base, relay_peer)| {
        let mut addr = base;
        addr.push(Protocol::P2p(relay_peer));
        addr.push(Protocol::P2pCircuit);
        addr
    })
}

// ─── State generators ───────────────────────────────────────────────────────

pub fn arb_phase() -> impl Strategy<Value = Phase> {
    prop_oneof![
        Just(Phase::Joining),
        Just(Phase::Ready),
        Just(Phase::ShuttingDown),
    ]
}

pub fn arb_nat_status() -> impl Strategy<Value = NatStatus> {
    prop_oneof![
        Just(NatStatus::Unknown),
        Just(NatStatus::Public),
        Just(NatStatus::Private),
    ]
}

// ─── Service / spec generators ──────────────────────────────────────────────

pub fn arb_service_name() -> impl Strategy<Value = ServiceName> {
    "[a-z][a-z0-9_-]{0,19}".prop_map(ServiceName::new)
}

pub fn arb_service_name_string() -> impl Strategy<Value = String> {
    "[a-z][a-z0-9_-]{0,19}".prop_map(String::from)
}

pub fn arb_ipv4_host() -> impl Strategy<Value = String> {
    (1u8..=254, 0u8..=255, 0u8..=255, 1u8..=254).prop_map(|(a, b, c, d)| format!("{a}.{b}.{c}.{d}"))
}

pub fn arb_domain_host() -> impl Strategy<Value = String> {
    "[a-z][a-z0-9-]{0,9}(\\.[a-z]{2,4}){0,2}"
}

pub fn arb_ipv6_host() -> impl Strategy<Value = String> {
    prop_oneof![Just("::1".to_string()), Just("fe80::1".to_string()),]
}

pub fn arb_host_addr() -> impl Strategy<Value = HostAddr> {
    prop_oneof![
        arb_ipv4_host().prop_map(|s| s.parse::<HostAddr>().expect("generated IPv4 is valid")),
        arb_ipv6_host().prop_map(|s| s.parse::<HostAddr>().expect("generated IPv6 is valid")),
        arb_domain_host().prop_map(|s| s.parse::<HostAddr>().expect("generated domain is valid")),
    ]
}

pub fn arb_port() -> impl Strategy<Value = u16> {
    1024u16..65535u16
}

pub fn arb_service_addr() -> impl Strategy<Value = crate::specs::ServiceAddr> {
    (arb_host_addr(), arb_port()).prop_map(|(host, port)| crate::specs::ServiceAddr { host, port })
}

pub fn arb_socket_addr() -> impl Strategy<Value = SocketAddr> {
    (
        1u8..=254,
        0u8..=255,
        0u8..=255,
        1u8..=254,
        1024u16..65535u16,
    )
        .prop_map(|(a, b, c, d, port)| {
            // Infallible: formatted string is always a valid socket address
            format!("{a}.{b}.{c}.{d}:{port}")
                .parse()
                .expect("generated socket address is always valid")
        })
}
