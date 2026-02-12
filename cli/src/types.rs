use std::{
    fmt,
    net::{Ipv4Addr, Ipv6Addr},
    str::FromStr,
    sync::Arc,
};

use libp2p::kad;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

use crate::protocol::SERVICE_KEY_PREFIX;

// ─── ServiceName ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ServiceName(Arc<str>);

impl Serialize for ServiceName {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ServiceName {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Ok(Self(s.into()))
    }
}

impl ServiceName {
    pub fn new(name: impl Into<Arc<str>>) -> Self {
        Self(name.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ServiceName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for ServiceName {
    type Err = ServiceNameParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.is_empty() {
            true => Err(ServiceNameParseError),
            false => Ok(Self(s.into())),
        }
    }
}

#[derive(Debug, Error)]
#[error("service name cannot be empty")]
pub struct ServiceNameParseError;

// ─── KademliaKey ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KademliaKey(Vec<u8>);

impl KademliaKey {
    pub fn for_service(name: &ServiceName) -> Self {
        Self(format!("{SERVICE_KEY_PREFIX}{}", name.as_str()).into_bytes())
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub fn into_record_key(self) -> kad::RecordKey {
        kad::RecordKey::new(&self.0)
    }
}

impl AsRef<[u8]> for KademliaKey {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

pub fn service_name_from_key(key: &[u8]) -> Option<&str> {
    std::str::from_utf8(key)
        .ok()
        .and_then(|s| s.strip_prefix(SERVICE_KEY_PREFIX))
}

// ─── HostAddr ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostAddr {
    V4(Ipv4Addr),
    V6(Ipv6Addr),
    Domain(String),
}

#[derive(Debug, Error)]
pub enum HostAddrParseError {
    #[error("empty host")]
    EmptyHost,
}

impl FromStr for HostAddr {
    type Err = HostAddrParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.is_empty() {
            true => Err(HostAddrParseError::EmptyHost),
            false => {
                if let Ok(v4) = s.parse::<Ipv4Addr>() {
                    return Ok(HostAddr::V4(v4));
                }
                if let Ok(v6) = s.parse::<Ipv6Addr>() {
                    return Ok(HostAddr::V6(v6));
                }
                Ok(HostAddr::Domain(s.to_string()))
            }
        }
    }
}

impl fmt::Display for HostAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HostAddr::V4(ip) => write!(f, "{ip}"),
            HostAddr::V6(ip) => write!(f, "{ip}"),
            HostAddr::Domain(d) => write!(f, "{d}"),
        }
    }
}

impl HostAddr {
    pub fn is_ipv6(&self) -> bool {
        matches!(self, HostAddr::V6(_))
    }
}
