use std::{fmt, net::SocketAddr, str::FromStr};

use anyhow::Result;
use thiserror::Error;

use crate::protocol::SERVICE_KEY_PREFIX;

// ─── ServiceAddr ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceAddr {
    pub host: String,
    pub port: u16,
}

#[derive(Debug, Error)]
pub enum ServiceAddrParseError {
    #[error("missing port separator ':'")]
    MissingPort,
    #[error("empty host")]
    EmptyHost,
    #[error("invalid port: {0}")]
    InvalidPort(#[from] std::num::ParseIntError),
}

impl FromStr for ServiceAddr {
    type Err = ServiceAddrParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (host, port_str) = if s.starts_with('[') {
            // IPv6 bracket notation: [::1]:8080
            let (bracket_host, rest) = s
                .split_once("]:")
                .ok_or(ServiceAddrParseError::MissingPort)?;
            // Infallible: starts_with('[') guarantees at least one byte to skip
            (&bracket_host[1..], rest)
        } else {
            s.rsplit_once(':')
                .ok_or(ServiceAddrParseError::MissingPort)?
        };

        if host.is_empty() {
            return Err(ServiceAddrParseError::EmptyHost);
        }

        let port: u16 = port_str.parse()?;

        Ok(Self {
            host: host.to_string(),
            port,
        })
    }
}

impl fmt::Display for ServiceAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.host.contains(':') {
            write!(f, "[{}]:{}", self.host, self.port)
        } else {
            write!(f, "{}:{}", self.host, self.port)
        }
    }
}

impl From<SocketAddr> for ServiceAddr {
    fn from(addr: SocketAddr) -> Self {
        Self {
            host: addr.ip().to_string(),
            port: addr.port(),
        }
    }
}

impl ServiceAddr {
    pub fn connect_tuple(&self) -> (&str, u16) {
        (&self.host, self.port)
    }
}

// ─── ExposedService ──────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ExposedService {
    pub name: String,
    pub local_addr: ServiceAddr,
}

#[derive(Debug, Error)]
pub enum ExposeParseError {
    #[error("expose format: name=host:port (missing '=')")]
    MissingSeparator,
    #[error("service name cannot be empty")]
    EmptyName,
    #[error("invalid address: {0}")]
    InvalidAddr(#[from] ServiceAddrParseError),
}

impl FromStr for ExposedService {
    type Err = ExposeParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (name, addr_str) = s
            .split_once('=')
            .ok_or(ExposeParseError::MissingSeparator)?;

        if name.is_empty() {
            return Err(ExposeParseError::EmptyName);
        }

        let local_addr: ServiceAddr = addr_str.parse()?;

        Ok(Self {
            name: name.to_string(),
            local_addr,
        })
    }
}

pub fn parse_expose(spec: &str) -> Result<ExposedService> {
    spec.parse().map_err(Into::into)
}

// ─── TunnelByNameSpec ────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct TunnelByNameSpec {
    pub service_name: String,
    pub bind_addr: ServiceAddr,
}

#[derive(Debug, Error)]
pub enum TunnelByNameParseError {
    #[error("tunnel-by-name format: service_name@bind_addr (missing '@')")]
    MissingSeparator,
    #[error("service name cannot be empty")]
    EmptyName,
    #[error("invalid bind address: {0}")]
    InvalidAddr(#[from] ServiceAddrParseError),
}

impl FromStr for TunnelByNameSpec {
    type Err = TunnelByNameParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (name, addr_str) = s
            .rsplit_once('@')
            .ok_or(TunnelByNameParseError::MissingSeparator)?;

        if name.is_empty() {
            return Err(TunnelByNameParseError::EmptyName);
        }

        let bind_addr: ServiceAddr = addr_str.parse()?;

        Ok(Self {
            service_name: name.to_string(),
            bind_addr,
        })
    }
}

pub fn parse_tunnel_by_name(spec: &str) -> Result<TunnelByNameSpec> {
    spec.parse().map_err(Into::into)
}

// ─── Service key helpers ─────────────────────────────────────────────────────

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
    use std::net::SocketAddr;

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

    fn arb_ipv4_host() -> impl Strategy<Value = String> {
        (1u8..=254, 0u8..=255, 0u8..=255, 1u8..=254)
            .prop_map(|(a, b, c, d)| format!("{a}.{b}.{c}.{d}"))
    }

    fn arb_domain_host() -> impl Strategy<Value = String> {
        "[a-z][a-z0-9-]{0,9}(\\.[a-z]{2,4}){0,2}"
    }

    fn arb_ipv6_host() -> impl Strategy<Value = String> {
        prop_oneof![Just("::1".to_string()), Just("fe80::1".to_string()),]
    }

    fn arb_port() -> impl Strategy<Value = u16> {
        1024u16..65535u16
    }

    proptest! {
        // Roundtrip: ServiceAddr with IPv4
        #[test]
        fn service_addr_ipv4_roundtrip(host in arb_ipv4_host(), port in arb_port()) {
            let addr = ServiceAddr { host: host.clone(), port };
            let s = addr.to_string();
            let parsed: ServiceAddr = s.parse().expect("parse generated ServiceAddr");
            prop_assert_eq!(parsed.host, host);
            prop_assert_eq!(parsed.port, port);
        }

        // Roundtrip: ServiceAddr with domain
        #[test]
        fn service_addr_domain_roundtrip(host in arb_domain_host(), port in arb_port()) {
            let addr = ServiceAddr { host: host.clone(), port };
            let s = addr.to_string();
            let parsed: ServiceAddr = s.parse().expect("parse generated ServiceAddr");
            prop_assert_eq!(parsed.host, host);
            prop_assert_eq!(parsed.port, port);
        }

        // Roundtrip: ServiceAddr with IPv6 (bracket notation)
        #[test]
        fn service_addr_ipv6_roundtrip(host in arb_ipv6_host(), port in arb_port()) {
            let addr = ServiceAddr { host: host.clone(), port };
            let s = addr.to_string();
            prop_assert!(s.starts_with('['), "IPv6 display should use brackets");
            let parsed: ServiceAddr = s.parse().expect("parse generated ServiceAddr");
            prop_assert_eq!(parsed.host, host);
            prop_assert_eq!(parsed.port, port);
        }

        // ServiceAddr from SocketAddr roundtrip
        #[test]
        fn service_addr_from_socket_addr(addr in arb_socket_addr()) {
            let sa = ServiceAddr::from(addr);
            prop_assert_eq!(sa.host, addr.ip().to_string());
            prop_assert_eq!(sa.port, addr.port());
        }

        // Roundtrip: ExposedService FromStr
        #[test]
        fn exposed_service_roundtrip(
            name in arb_service_name(),
            host in arb_ipv4_host(),
            port in arb_port(),
        ) {
            let spec = format!("{name}={host}:{port}");
            let svc: ExposedService = spec.parse().expect("parse generated expose spec");
            prop_assert_eq!(svc.name, name);
            prop_assert_eq!(svc.local_addr.host, host);
            prop_assert_eq!(svc.local_addr.port, port);
        }

        // Roundtrip: ExposedService with domain
        #[test]
        fn exposed_service_domain_roundtrip(
            name in arb_service_name(),
            host in arb_domain_host(),
            port in arb_port(),
        ) {
            let spec = format!("{name}={host}:{port}");
            let svc: ExposedService = spec.parse().expect("parse generated expose spec");
            prop_assert_eq!(svc.name, name);
            prop_assert_eq!(svc.local_addr.host, host);
            prop_assert_eq!(svc.local_addr.port, port);
        }

        // parse_expose wrapper delegates to FromStr
        #[test]
        fn parse_expose_roundtrip(
            name in arb_service_name(),
            addr in arb_socket_addr(),
        ) {
            let spec = format!("{name}={addr}");
            let svc = parse_expose(&spec).expect("parse generated expose spec");
            prop_assert_eq!(svc.name, name);
            prop_assert_eq!(svc.local_addr.host, addr.ip().to_string());
            prop_assert_eq!(svc.local_addr.port, addr.port());
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

        // Roundtrip: TunnelByNameSpec FromStr with IP address
        #[test]
        fn tunnel_by_name_spec_roundtrip(
            name in arb_service_name(),
            addr in arb_socket_addr(),
        ) {
            let spec = format!("{name}@{addr}");
            let parsed: TunnelByNameSpec = spec.parse()
                .expect("parse generated tunnel-by-name spec");
            prop_assert_eq!(parsed.service_name, name);
            prop_assert_eq!(parsed.bind_addr.host, addr.ip().to_string());
            prop_assert_eq!(parsed.bind_addr.port, addr.port());
        }

        // Roundtrip: TunnelByNameSpec FromStr with domain name
        #[test]
        fn tunnel_by_name_domain_roundtrip(
            name in arb_service_name(),
            host in arb_domain_host(),
            port in arb_port(),
        ) {
            let spec = format!("{name}@{host}:{port}");
            let parsed: TunnelByNameSpec = spec.parse()
                .expect("parse generated tunnel-by-name spec with domain");
            prop_assert_eq!(parsed.service_name, name);
            prop_assert_eq!(parsed.bind_addr.host, host);
            prop_assert_eq!(parsed.bind_addr.port, port);
        }

        // parse_tunnel_by_name wrapper delegates to FromStr
        #[test]
        fn parse_tunnel_by_name_roundtrip(
            name in arb_service_name(),
            addr in arb_socket_addr(),
        ) {
            let spec = format!("{name}@{addr}");
            let parsed = parse_tunnel_by_name(&spec)
                .expect("parse generated tunnel-by-name spec");
            prop_assert_eq!(parsed.service_name, name);
            prop_assert_eq!(parsed.bind_addr.host, addr.ip().to_string());
            prop_assert_eq!(parsed.bind_addr.port, addr.port());
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
