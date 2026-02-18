use std::{net::SocketAddr, time::Duration};

use anyhow::{Context, Result};
use stun::{
    agent::TransactionId,
    message::{BINDING_REQUEST, Getter, Message},
    xoraddr::XorMappedAddress,
};
use tokio::net::UdpSocket;
use tracing::warn;

pub const DEFAULT_STUN_SERVERS: &[&str] = &["stun.l.google.com:19302", "stun.cloudflare.com:3478"];

pub const STUN_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, strum::Display)]
pub enum NatMapping {
    #[strum(serialize = "Endpoint-Independent")]
    EndpointIndependent,
    #[strum(serialize = "Address-Dependent")]
    AddressDependent,
    #[strum(serialize = "Unknown")]
    Unknown,
}

impl NatMapping {
    pub fn is_holepunch_viable(self) -> bool {
        match self {
            NatMapping::EndpointIndependent => true,
            NatMapping::AddressDependent => false,
            NatMapping::Unknown => true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, strum::Display)]
pub enum NatFiltering {
    #[strum(serialize = "Endpoint-Independent")]
    EndpointIndependent,
    #[strum(serialize = "Restricted")]
    Restricted,
    #[strum(serialize = "Unknown")]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NatClassification {
    pub mapping: NatMapping,
    pub filtering: NatFiltering,
}

impl NatClassification {
    pub fn is_holepunch_viable(self) -> bool {
        self.mapping.is_holepunch_viable()
    }
}

pub async fn detect_nat_mapping(stun_servers: &[&str]) -> NatMapping {
    match probe_stun_servers(stun_servers).await {
        Ok(addrs) if addrs.len() >= 2 => classify_mapping(&addrs),
        Ok(_) => {
            warn!("fewer than 2 STUN responses received, cannot classify NAT mapping");
            NatMapping::Unknown
        }
        Err(e) => {
            warn!("STUN probe failed: {e:#}");
            NatMapping::Unknown
        }
    }
}

pub async fn probe_filtering(
    mut control: libp2p_stream::Control,
    bootstrap_peer: libp2p::PeerId,
    stun_server: &str,
) -> NatFiltering {
    match probe_filtering_inner(&mut control, bootstrap_peer, stun_server).await {
        Ok(filtering) => filtering,
        Err(e) => {
            warn!("NAT filtering probe failed: {e:#}");
            NatFiltering::Unknown
        }
    }
}

async fn probe_filtering_inner(
    control: &mut libp2p_stream::Control,
    bootstrap_peer: libp2p::PeerId,
    stun_server: &str,
) -> Result<NatFiltering> {
    use futures::io::{AsyncReadExt, AsyncWriteExt};

    let socket = UdpSocket::bind("0.0.0.0:0")
        .await
        .context("failed to bind probe UDP socket")?;

    let mapped_addr = stun_binding_request(&socket, stun_server).await?;
    tracing::info!(%mapped_addr, "filtering probe: learned mapped address via STUN");

    let mut stream = control
        .open_stream(bootstrap_peer, crate::protocol::nat_probe_protocol())
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))
        .context("failed to open NAT probe stream to bootstrap")?;

    let request = crate::nat_probe_protocol::NatProbeRequest { mapped_addr };
    let data = serde_json::to_vec(&request)?;
    let len = u32::try_from(data.len()).context("probe request too large")?;
    stream.write_all(&len.to_be_bytes()).await?;
    stream.write_all(&data).await?;
    stream.flush().await?;

    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await?;
    // Infallible: u32 always fits in usize on all Rust std-supported platforms (>= 32-bit)
    let resp_len = usize::try_from(u32::from_be_bytes(len_buf))
        .expect("u32 fits in usize on all supported platforms");
    anyhow::ensure!(resp_len <= 1024, "probe response too large: {resp_len}");
    let mut resp_buf = vec![0u8; resp_len];
    stream.read_exact(&mut resp_buf).await?;

    let response: crate::nat_probe_protocol::NatProbeResponse = serde_json::from_slice(&resp_buf)?;
    anyhow::ensure!(response.probe_sent, "bootstrap refused to send probe");

    let mut buf = [0u8; 64];
    match tokio::time::timeout(STUN_TIMEOUT, socket.recv_from(&mut buf)).await {
        Ok(Ok((_, src))) => {
            tracing::info!(%src, "filtering probe: received UDP probe — full cone NAT");
            Ok(NatFiltering::EndpointIndependent)
        }
        Ok(Err(e)) => {
            tracing::info!(error = %e, "filtering probe: recv error");
            Ok(NatFiltering::Unknown)
        }
        Err(_) => {
            tracing::info!("filtering probe: no probe received — restricted filtering NAT");
            Ok(NatFiltering::Restricted)
        }
    }
}

fn classify_mapping(addrs: &[SocketAddr]) -> NatMapping {
    let all_same_port = addrs
        .first()
        .map(|first| addrs.iter().all(|addr| addr.port() == first.port()))
        .unwrap_or(true);

    if all_same_port {
        NatMapping::EndpointIndependent
    } else {
        NatMapping::AddressDependent
    }
}

async fn probe_stun_servers(servers: &[&str]) -> Result<Vec<SocketAddr>> {
    let socket = UdpSocket::bind("0.0.0.0:0")
        .await
        .context("failed to bind UDP socket")?;

    let mut results = Vec::new();
    for server in servers {
        match stun_binding_request(&socket, server).await {
            Ok(addr) => {
                tracing::info!("STUN response from {server}: {addr}");
                results.push(addr);
            }
            Err(e) => {
                warn!("STUN request to {server} failed: {e:#}");
            }
        }
    }
    Ok(results)
}

async fn stun_binding_request(socket: &UdpSocket, server: &str) -> Result<SocketAddr> {
    let mut msg = Message::new();
    msg.build(&[Box::new(TransactionId::new()), Box::new(BINDING_REQUEST)])
        .map_err(|e| anyhow::anyhow!("{e}"))
        .context("failed to build STUN binding request")?;

    let dest: SocketAddr = tokio::net::lookup_host(server)
        .await
        .context("DNS lookup failed")?
        .next()
        .context("no addresses resolved for STUN server")?;

    socket
        .send_to(&msg.raw, dest)
        .await
        .context("failed to send STUN request")?;

    let mut buf = [0u8; 1024];
    let (n, src) = tokio::time::timeout(STUN_TIMEOUT, socket.recv_from(&mut buf))
        .await
        .context("STUN request timed out")?
        .context("failed to receive STUN response")?;

    anyhow::ensure!(
        src == dest,
        "STUN response from unexpected source {src}, expected {dest}"
    );

    let mut resp = Message::new();
    resp.write(
        buf.get(..n)
            .context("response buffer slice out of bounds")?,
    )
    .map_err(|e| anyhow::anyhow!("{e}"))
    .context("failed to decode STUN response")?;

    let mut xor_addr = XorMappedAddress::default();
    xor_addr
        .get_from(&resp)
        .map_err(|e| anyhow::anyhow!("{e}"))
        .context("failed to extract XOR-MAPPED-ADDRESS")?;

    Ok(SocketAddr::new(xor_addr.ip, xor_addr.port))
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    use proptest::prelude::*;

    use super::*;

    #[test]
    fn endpoint_independent_is_holepunch_viable() {
        assert!(NatMapping::EndpointIndependent.is_holepunch_viable());
    }

    #[test]
    fn address_dependent_is_not_holepunch_viable() {
        assert!(!NatMapping::AddressDependent.is_holepunch_viable());
    }

    #[test]
    fn unknown_is_optimistically_holepunch_viable() {
        assert!(NatMapping::Unknown.is_holepunch_viable());
    }

    #[test]
    fn display_formats_are_human_readable() {
        assert_eq!(
            NatMapping::EndpointIndependent.to_string(),
            "Endpoint-Independent"
        );
        assert_eq!(
            NatMapping::AddressDependent.to_string(),
            "Address-Dependent"
        );
        assert_eq!(NatMapping::Unknown.to_string(), "Unknown");
    }

    #[test]
    fn classify_empty_as_endpoint_independent() {
        assert_eq!(classify_mapping(&[]), NatMapping::EndpointIndependent);
    }

    fn arb_ipv4() -> impl Strategy<Value = Ipv4Addr> {
        (1u8..=254, any::<u8>(), any::<u8>(), 1u8..=254)
            .prop_map(|(a, b, c, d)| Ipv4Addr::new(a, b, c, d))
    }

    fn arb_port() -> impl Strategy<Value = u16> {
        1u16..=65534
    }

    fn arb_socket_addr_with_port(port: u16) -> impl Strategy<Value = SocketAddr> {
        arb_ipv4().prop_map(move |ip| SocketAddr::new(IpAddr::V4(ip), port))
    }

    proptest! {
        #[test]
        fn classify_same_ports_as_endpoint_independent(
            port in arb_port(),
            ip1 in arb_ipv4(),
            ip2 in arb_ipv4(),
        ) {
            let addrs = vec![
                SocketAddr::new(IpAddr::V4(ip1), port),
                SocketAddr::new(IpAddr::V4(ip2), port),
            ];
            prop_assert_eq!(classify_mapping(&addrs), NatMapping::EndpointIndependent);
        }

        #[test]
        fn classify_different_ports_as_address_dependent(
            ip1 in arb_ipv4(),
            ip2 in arb_ipv4(),
            port1 in arb_port(),
            port2 in arb_port(),
        ) {
            prop_assume!(port1 != port2);
            let addrs = vec![
                SocketAddr::new(IpAddr::V4(ip1), port1),
                SocketAddr::new(IpAddr::V4(ip2), port2),
            ];
            prop_assert_eq!(classify_mapping(&addrs), NatMapping::AddressDependent);
        }

        #[test]
        fn classify_single_addr_as_endpoint_independent(
            addr in arb_socket_addr_with_port(12345u16),
        ) {
            prop_assert_eq!(classify_mapping(&[addr]), NatMapping::EndpointIndependent);
        }

        #[test]
        fn classify_many_same_ports_as_endpoint_independent(
            port in arb_port(),
            ips in prop::collection::vec(arb_ipv4(), 2..10),
        ) {
            let addrs: Vec<SocketAddr> = ips
                .into_iter()
                .map(|ip| SocketAddr::new(IpAddr::V4(ip), port))
                .collect();
            prop_assert_eq!(classify_mapping(&addrs), NatMapping::EndpointIndependent);
        }

        #[test]
        fn classification_holepunch_viable_requires_ei_mapping(
            filtering in prop_oneof![
                Just(NatFiltering::EndpointIndependent),
                Just(NatFiltering::Restricted),
                Just(NatFiltering::Unknown),
            ],
        ) {
            let class = NatClassification {
                mapping: NatMapping::AddressDependent,
                filtering,
            };
            prop_assert!(!class.is_holepunch_viable());
        }

        #[test]
        fn classification_full_cone_always_viable(
            mapping in prop_oneof![
                Just(NatMapping::EndpointIndependent),
                Just(NatMapping::Unknown),
            ],
        ) {
            let class = NatClassification {
                mapping,
                filtering: NatFiltering::EndpointIndependent,
            };
            prop_assert!(class.is_holepunch_viable());
        }
    }

    #[test]
    fn endpoint_independent_filtering_display() {
        assert_eq!(
            NatFiltering::EndpointIndependent.to_string(),
            "Endpoint-Independent"
        );
    }

    #[test]
    fn restricted_filtering_display() {
        assert_eq!(NatFiltering::Restricted.to_string(), "Restricted");
    }

    #[test]
    fn classification_combines_mapping_and_filtering() {
        let class = NatClassification {
            mapping: NatMapping::EndpointIndependent,
            filtering: NatFiltering::Restricted,
        };
        assert_eq!(class.mapping, NatMapping::EndpointIndependent);
        assert_eq!(class.filtering, NatFiltering::Restricted);
    }
}
