use std::net::Ipv4Addr;

use libp2p::{Multiaddr, multiaddr::Protocol};

fn needs_external_rewrite(ip: Ipv4Addr) -> bool {
    ip.is_unspecified() || ip.is_private() || ip.is_loopback()
}

fn is_circuit_addr(addr: &Multiaddr) -> bool {
    addr.iter().any(|p| matches!(p, Protocol::P2pCircuit))
}

pub fn has_public_ip(addr: &Multiaddr) -> bool {
    addr.iter().any(|p| match p {
        Protocol::Ip4(ip) => !needs_external_rewrite(ip),
        Protocol::Ip6(ip) => !ip.is_loopback() && !ip.is_unspecified(),
        _ => false,
    })
}

pub fn is_valid_external_candidate(addr: &Multiaddr) -> bool {
    !is_circuit_addr(addr) && has_public_ip(addr)
}

pub fn extract_public_ip_port(addr: &Multiaddr) -> Option<(std::net::IpAddr, u16)> {
    let mut ip = None;
    let mut port = None;
    for proto in addr.iter() {
        match proto {
            Protocol::Ip4(a) if !needs_external_rewrite(a) => {
                ip = Some(std::net::IpAddr::V4(a));
            }
            Protocol::Ip6(a) if !a.is_loopback() && !a.is_unspecified() => {
                ip = Some(std::net::IpAddr::V6(a));
            }
            Protocol::Udp(p) | Protocol::Tcp(p) => port = Some(p),
            _ => {}
        }
    }
    ip.zip(port)
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use libp2p::Multiaddr;
    use proptest::prelude::*;

    use super::*;

    fn arb_external_ipv4() -> impl Strategy<Value = Ipv4Addr> {
        (1u8..=223, any::<u8>(), any::<u8>(), 1u8..=254)
            .prop_filter("must be public IP", |&(a, b, ..)| {
                let ip = Ipv4Addr::new(a, b, 0, 0);
                !ip.is_private() && !ip.is_loopback() && !ip.is_unspecified()
            })
            .prop_map(|(a, b, c, d)| Ipv4Addr::new(a, b, c, d))
    }

    fn arb_private_ipv4() -> impl Strategy<Value = Ipv4Addr> {
        prop_oneof![
            (any::<u8>(), any::<u8>(), any::<u8>())
                .prop_map(|(b, c, d)| Ipv4Addr::new(10, b, c, d)),
            (16u8..=31, any::<u8>(), any::<u8>()).prop_map(|(b, c, d)| Ipv4Addr::new(172, b, c, d)),
            (any::<u8>(), any::<u8>()).prop_map(|(c, d)| Ipv4Addr::new(192, 168, c, d)),
        ]
    }

    fn arb_port() -> impl Strategy<Value = u16> {
        1u16..=65534
    }

    proptest! {
        #[test]
        fn needs_rewrite_for_all_private_ranges(ip in arb_private_ipv4()) {
            prop_assert!(needs_external_rewrite(ip));
        }

        #[test]
        fn no_rewrite_for_public_ips(ip in arb_external_ipv4()) {
            prop_assert!(!needs_external_rewrite(ip));
        }

        #[test]
        fn valid_candidate_accepts_public_direct_addr(
            ip in arb_external_ipv4(),
            port in arb_port(),
        ) {
            let addr: Multiaddr = format!("/ip4/{ip}/udp/{port}/quic-v1")
                .parse().expect("static format produces valid multiaddr");
            prop_assert!(is_valid_external_candidate(&addr));
        }

        #[test]
        fn valid_candidate_rejects_circuit_addr(
            ip in arb_external_ipv4(),
            port in arb_port(),
        ) {
            let addr: Multiaddr = format!(
                "/ip4/{ip}/udp/{port}/quic-v1/p2p/12D3KooWDpJ7As7BWAwRMfu1VU2WCqNjvq387JEYKDBj4kx6nXTN/p2p-circuit"
            ).parse().expect("static circuit multiaddr is valid");
            prop_assert!(!is_valid_external_candidate(&addr));
        }

        #[test]
        fn valid_candidate_rejects_private_ip(
            ip in arb_private_ipv4(),
            port in arb_port(),
        ) {
            let addr: Multiaddr = format!("/ip4/{ip}/udp/{port}/quic-v1")
                .parse().expect("static format produces valid multiaddr");
            prop_assert!(!is_valid_external_candidate(&addr));
        }

        #[test]
        fn valid_candidate_rejects_bare_peer_id(
            ip in arb_external_ipv4(),
        ) {
            // Bare /p2p/ address has no IP — should be rejected
            let _ = ip; // use the generated value to satisfy proptest
            let addr: Multiaddr = "/p2p/12D3KooWDpJ7As7BWAwRMfu1VU2WCqNjvq387JEYKDBj4kx6nXTN"
                .parse().expect("static peer ID multiaddr is valid");
            prop_assert!(!is_valid_external_candidate(&addr));
        }
    }
}
