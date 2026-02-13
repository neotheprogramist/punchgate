use std::{
    net::{IpAddr, Ipv4Addr},
    time::Duration,
};

use libp2p::{Multiaddr, multiaddr::Protocol};

const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(10);

const DISCOVERY_URLS: &[&str] = &[
    "https://ifconfig.me",
    "https://api.ipify.org",
    "https://icanhazip.com",
];

pub async fn discover_external_ip() -> Option<IpAddr> {
    let client = reqwest::Client::builder()
        .timeout(DISCOVERY_TIMEOUT)
        .build()
        .ok()?;

    for url in DISCOVERY_URLS {
        match client
            .get(*url)
            .send()
            .await
            .and_then(|r| r.error_for_status())
        {
            Ok(resp) => match resp.text().await {
                Ok(body) => match body.trim().parse::<IpAddr>() {
                    Ok(ip) => {
                        tracing::info!(%ip, service = %url, "discovered external IP");
                        return Some(ip);
                    }
                    Err(e) => {
                        tracing::debug!(service = %url, body = %body.trim(), error = %e, "failed to parse IP");
                    }
                },
                Err(e) => {
                    tracing::debug!(service = %url, error = %e, "failed to read response body");
                }
            },
            Err(e) => {
                tracing::debug!(service = %url, error = %e, "external IP discovery request failed");
            }
        }
    }
    tracing::warn!("could not discover external IP from any service");
    None
}

fn needs_external_rewrite(ip: Ipv4Addr) -> bool {
    ip.is_unspecified() || ip.is_private() || ip.is_loopback()
}

pub fn make_external_addr(listen_addr: &Multiaddr, external_ip: IpAddr) -> Option<Multiaddr> {
    let mut rewritten = false;
    let new_addr = listen_addr
        .iter()
        .map(|proto| match proto {
            Protocol::Ip4(ip) if needs_external_rewrite(ip) => {
                rewritten = true;
                match external_ip {
                    IpAddr::V4(v4) => Protocol::Ip4(v4),
                    IpAddr::V6(v6) => Protocol::Ip6(v6),
                }
            }
            other => other,
        })
        .collect::<Multiaddr>();

    rewritten.then_some(new_addr)
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

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
        fn rewrites_private_ip_in_tcp_addr(
            private_ip in arb_private_ipv4(),
            external_ip in arb_external_ipv4(),
            port in arb_port(),
        ) {
            let listen: Multiaddr = format!("/ip4/{private_ip}/tcp/{port}")
                .parse()
                .expect("static format produces valid multiaddr");
            let result = make_external_addr(&listen, IpAddr::V4(external_ip));
            let expected: Multiaddr = format!("/ip4/{external_ip}/tcp/{port}")
                .parse()
                .expect("static format produces valid multiaddr");
            prop_assert_eq!(result, Some(expected));
        }

        #[test]
        fn rewrites_unspecified_ip_in_tcp_addr(
            external_ip in arb_external_ipv4(),
            port in arb_port(),
        ) {
            let listen: Multiaddr = format!("/ip4/0.0.0.0/tcp/{port}")
                .parse()
                .expect("static format produces valid multiaddr");
            let result = make_external_addr(&listen, IpAddr::V4(external_ip));
            let expected: Multiaddr = format!("/ip4/{external_ip}/tcp/{port}")
                .parse()
                .expect("static format produces valid multiaddr");
            prop_assert_eq!(result, Some(expected));
        }

        #[test]
        fn rewrites_private_ip_in_quic_addr(
            private_ip in arb_private_ipv4(),
            external_ip in arb_external_ipv4(),
            port in arb_port(),
        ) {
            let listen: Multiaddr = format!("/ip4/{private_ip}/udp/{port}/quic-v1")
                .parse()
                .expect("static format produces valid multiaddr");
            let result = make_external_addr(&listen, IpAddr::V4(external_ip));
            let expected: Multiaddr = format!("/ip4/{external_ip}/udp/{port}/quic-v1")
                .parse()
                .expect("static format produces valid multiaddr");
            prop_assert_eq!(result, Some(expected));
        }

        #[test]
        fn skips_already_public_ip(
            external_ip in arb_external_ipv4(),
            listen_ip in arb_external_ipv4(),
            port in arb_port(),
        ) {
            let listen: Multiaddr = format!("/ip4/{listen_ip}/tcp/{port}")
                .parse()
                .expect("static format produces valid multiaddr");
            let result = make_external_addr(&listen, IpAddr::V4(external_ip));
            prop_assert_eq!(result, None);
        }

        #[test]
        fn needs_rewrite_for_all_private_ranges(ip in arb_private_ipv4()) {
            prop_assert!(needs_external_rewrite(ip));
        }

        #[test]
        fn no_rewrite_for_public_ips(ip in arb_external_ipv4()) {
            prop_assert!(!needs_external_rewrite(ip));
        }
    }
}
