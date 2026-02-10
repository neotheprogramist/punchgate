use std::net::SocketAddr;

use anyhow::{Context, Result, bail};

use crate::protocol::SERVICE_KEY_PREFIX;

#[derive(Debug, Clone)]
pub struct ExposedService {
    pub name: String,
    pub local_addr: SocketAddr,
}

/// Parse an `--expose` spec: `name=host:port`
pub fn parse_expose(spec: &str) -> Result<ExposedService> {
    let (name, addr_str) = spec
        .split_once('=')
        .context("expose format: name=host:port")?;

    if name.is_empty() {
        bail!("service name cannot be empty");
    }

    let local_addr: SocketAddr = addr_str
        .parse()
        .with_context(|| format!("invalid address: {addr_str}"))?;

    Ok(ExposedService {
        name: name.to_string(),
        local_addr,
    })
}

/// Parse a `--tunnel-by-name` spec: `service_name@bind_addr`
pub fn parse_tunnel_by_name(spec: &str) -> Result<(String, SocketAddr)> {
    let (name, addr_str) = spec
        .rsplit_once('@')
        .context("tunnel-by-name format: service_name@bind_addr")?;

    if name.is_empty() {
        bail!("service name cannot be empty");
    }

    let addr: SocketAddr = addr_str
        .parse()
        .with_context(|| format!("invalid bind address: {addr_str}"))?;

    Ok((name.to_string(), addr))
}

pub fn service_key(name: &str) -> Vec<u8> {
    format!("{SERVICE_KEY_PREFIX}{name}").into_bytes()
}

pub fn service_name_from_key(key: &[u8]) -> Option<&str> {
    std::str::from_utf8(key)
        .ok()
        .and_then(|s| s.strip_prefix(SERVICE_KEY_PREFIX))
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn arb_service_name() -> impl Strategy<Value = String> {
        "[a-z][a-z0-9_-]{0,19}"
    }

    fn arb_socket_addr() -> impl Strategy<Value = SocketAddr> {
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

    proptest! {
        // Roundtrip: parse_expose recovers name and address from formatted spec
        #[test]
        fn parse_expose_roundtrip(
            name in arb_service_name(),
            addr in arb_socket_addr(),
        ) {
            let spec = format!("{name}={addr}");
            let svc = parse_expose(&spec).expect("parse generated expose spec");
            prop_assert_eq!(svc.name, name);
            prop_assert_eq!(svc.local_addr, addr);
        }

        // service_key always produces a UTF-8 string with the expected prefix
        #[test]
        fn service_key_format_contract(name in arb_service_name()) {
            let key = service_key(&name);
            let key_str = String::from_utf8(key)
                .expect("service key should always be valid UTF-8");
            prop_assert!(key_str.starts_with(SERVICE_KEY_PREFIX));
            prop_assert!(key_str.ends_with(&name));
        }

        // Roundtrip: parse_tunnel_by_name recovers name and address from formatted spec
        #[test]
        fn parse_tunnel_by_name_roundtrip(
            name in arb_service_name(),
            addr in arb_socket_addr(),
        ) {
            let spec = format!("{name}@{addr}");
            let (parsed_name, parsed_addr) = parse_tunnel_by_name(&spec)
                .expect("parse generated tunnel-by-name spec");
            prop_assert_eq!(parsed_name, name);
            prop_assert_eq!(parsed_addr, addr);
        }

        // Roundtrip: service_name_from_key recovers the name embedded by service_key
        #[test]
        fn service_name_from_key_roundtrip(name in arb_service_name()) {
            let key = service_key(&name);
            let recovered = service_name_from_key(&key);
            prop_assert_eq!(recovered, Some(name.as_str()));
        }
    }
}
