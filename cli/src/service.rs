use std::net::SocketAddr;

use anyhow::{Context, Result, bail};

use crate::protocol::SERVICE_KEY_PREFIX;

/// A local service exposed to the mesh.
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

/// Build a DHT key for a service name.
pub fn service_key(name: &str) -> Vec<u8> {
    format!("{SERVICE_KEY_PREFIX}{name}").into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_expose_valid() {
        let svc = parse_expose("myapp=127.0.0.1:8080").expect("parse valid expose spec");
        assert_eq!(svc.name, "myapp");
        assert_eq!(
            svc.local_addr,
            "127.0.0.1:8080"
                .parse::<SocketAddr>()
                .expect("parse socket address")
        );
    }

    #[test]
    fn parse_expose_missing_equals() {
        assert!(parse_expose("myapp127.0.0.1:8080").is_err());
    }

    #[test]
    fn parse_expose_empty_name() {
        assert!(parse_expose("=127.0.0.1:8080").is_err());
    }

    #[test]
    fn parse_expose_invalid_addr() {
        assert!(parse_expose("myapp=not_an_addr").is_err());
    }

    #[test]
    fn service_key_has_prefix() {
        let key = service_key("echo");
        let key_str = String::from_utf8(key).expect("service key is valid UTF-8");
        assert!(key_str.starts_with("/punchgate/svc/"));
        assert!(key_str.ends_with("echo"));
    }
}
