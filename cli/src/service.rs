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

pub fn service_key(name: &str) -> Vec<u8> {
    format!("{SERVICE_KEY_PREFIX}{name}").into_bytes()
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
    }
}
