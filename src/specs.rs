use std::{fmt, net::SocketAddr, str::FromStr};

use anyhow::Result;
use libp2p::PeerId;
use thiserror::Error;

use crate::types::{HostAddr, ServiceName};

// ─── ServiceAddr ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceAddr {
    pub host: HostAddr,
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
        let (host, port_str) = match s.strip_prefix('[') {
            Some(after_bracket) => {
                let (bracket_host, rest) = after_bracket
                    .split_once("]:")
                    .ok_or(ServiceAddrParseError::MissingPort)?;
                (bracket_host, rest)
            }
            None => s
                .rsplit_once(':')
                .ok_or(ServiceAddrParseError::MissingPort)?,
        };

        if host.is_empty() {
            return Err(ServiceAddrParseError::EmptyHost);
        }

        let port: u16 = port_str.parse()?;

        let host_addr: HostAddr = host.parse().map_err(|_| ServiceAddrParseError::EmptyHost)?;

        Ok(Self {
            host: host_addr,
            port,
        })
    }
}

impl fmt::Display for ServiceAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.host {
            HostAddr::V6(_) => write!(f, "[{}]:{}", self.host, self.port),
            HostAddr::V4(_) | HostAddr::Domain(_) => write!(f, "{}:{}", self.host, self.port),
        }
    }
}

impl From<SocketAddr> for ServiceAddr {
    fn from(addr: SocketAddr) -> Self {
        let host = match addr {
            SocketAddr::V4(v4) => HostAddr::V4(*v4.ip()),
            SocketAddr::V6(v6) => HostAddr::V6(*v6.ip()),
        };
        Self {
            host,
            port: addr.port(),
        }
    }
}

impl ServiceAddr {
    pub fn connect_tuple(&self) -> (String, u16) {
        (self.host.to_string(), self.port)
    }
}

// ─── ExposedService ─────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ExposedService {
    pub name: ServiceName,
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
            name: ServiceName::new(name),
            local_addr,
        })
    }
}

// ─── TunnelByNameSpec ───────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct TunnelByNameSpec {
    pub service_name: ServiceName,
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
            service_name: ServiceName::new(name),
            bind_addr,
        })
    }
}

// ─── TunnelSpec ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct TunnelSpec {
    pub remote_peer: PeerId,
    pub service_name: ServiceName,
    pub bind_addr: ServiceAddr,
}

#[derive(Debug, Error)]
pub enum TunnelSpecParseError {
    #[error("tunnel format: peer_id:service_name@bind_addr (missing '@')")]
    MissingBindSeparator,
    #[error("tunnel format: peer_id:service_name@bind_addr (missing ':')")]
    MissingPeerSeparator,
    #[error("invalid peer ID: {0}")]
    InvalidPeerId(String),
    #[error("invalid bind address: {0}")]
    InvalidBindAddr(#[from] ServiceAddrParseError),
}

impl FromStr for TunnelSpec {
    type Err = TunnelSpecParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (peer_and_service, bind_str) = s
            .rsplit_once('@')
            .ok_or(TunnelSpecParseError::MissingBindSeparator)?;

        let (peer_str, service_name) = peer_and_service
            .split_once(':')
            .ok_or(TunnelSpecParseError::MissingPeerSeparator)?;

        let remote_peer: PeerId = peer_str
            .parse()
            .map_err(|e| TunnelSpecParseError::InvalidPeerId(format!("{e}")))?;

        let bind_addr: ServiceAddr = bind_str.parse()?;

        Ok(Self {
            remote_peer,
            service_name: ServiceName::new(service_name),
            bind_addr,
        })
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::test_utils::*;

    proptest! {
        #[test]
        fn service_addr_roundtrip(host in arb_host_addr(), port in arb_port()) {
            let addr = ServiceAddr { host: host.clone(), port };
            let s = addr.to_string();
            let parsed: ServiceAddr = s.parse().expect("parse generated ServiceAddr");
            prop_assert_eq!(parsed.host, host);
            prop_assert_eq!(parsed.port, port);
        }

        #[test]
        fn service_addr_from_socket_addr(addr in arb_socket_addr()) {
            let sa = ServiceAddr::from(addr);
            let expected_host: HostAddr = addr.ip().to_string().parse()
                .expect("socket addr IP is always a valid HostAddr");
            prop_assert_eq!(sa.host, expected_host);
            prop_assert_eq!(sa.port, addr.port());
        }

        #[test]
        fn exposed_service_roundtrip(
            name in arb_service_name_string(),
            host in arb_host_addr(),
            port in arb_port(),
        ) {
            let bind = match host {
                HostAddr::V6(_) => format!("[{}]:{port}", host),
                HostAddr::V4(_) | HostAddr::Domain(_) => format!("{host}:{port}"),
            };
            let spec = format!("{name}={bind}");
            let svc: ExposedService = spec.parse().expect("parse generated expose spec");
            prop_assert_eq!(svc.name.as_str(), name.as_str());
            prop_assert_eq!(svc.local_addr.host, host);
            prop_assert_eq!(svc.local_addr.port, port);
        }

        #[test]
        fn tunnel_by_name_roundtrip(
            name in arb_service_name_string(),
            host in arb_host_addr(),
            port in arb_port(),
        ) {
            let bind = match host {
                HostAddr::V6(_) => format!("[{}]:{port}", host),
                HostAddr::V4(_) | HostAddr::Domain(_) => format!("{host}:{port}"),
            };
            let spec = format!("{name}@{bind}");
            let parsed: TunnelByNameSpec = spec.parse()
                .expect("parse generated tunnel-by-name spec");
            prop_assert_eq!(parsed.service_name.as_str(), name.as_str());
            prop_assert_eq!(parsed.bind_addr.host, host);
            prop_assert_eq!(parsed.bind_addr.port, port);
        }

        #[test]
        fn tunnel_spec_roundtrip(
            peer_id in arb_peer_id(),
            service in arb_service_name_string(),
            host in arb_host_addr(),
            port in arb_port(),
        ) {
            let bind = match host {
                HostAddr::V6(_) => format!("[{}]:{port}", host),
                HostAddr::V4(_) | HostAddr::Domain(_) => format!("{host}:{port}"),
            };
            let spec = format!("{peer_id}:{service}@{bind}");
            let parsed: TunnelSpec = spec.parse().expect("parse generated tunnel spec");
            prop_assert_eq!(parsed.remote_peer, peer_id);
            prop_assert_eq!(parsed.service_name.as_str(), service.as_str());
            prop_assert_eq!(parsed.bind_addr.host, host);
            prop_assert_eq!(parsed.bind_addr.port, port);
        }

        #[test]
        fn service_key_roundtrip(name in arb_service_name()) {
            let key = crate::types::KademliaKey::for_service(&name);
            let recovered = crate::types::service_name_from_key(key.as_bytes());
            prop_assert_eq!(recovered, Some(name.as_str()));
        }
    }
}
