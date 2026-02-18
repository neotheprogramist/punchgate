use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4},
    num::NonZeroU16,
    time::Duration,
};

use anyhow::{Context, Result};
use tracing::{info, warn};

const PORT_MAPPING_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy)]
pub struct PortMapping {
    pub external_addr: SocketAddr,
    pub protocol_used: &'static str,
}

fn detect_gateway() -> Result<Ipv4Addr> {
    let gw = default_net::get_default_gateway()
        .map_err(|e| anyhow::anyhow!(e))
        .context("failed to detect default gateway")?;

    match gw.ip_addr {
        IpAddr::V4(ipv4) => Ok(ipv4),
        IpAddr::V6(ipv6) => anyhow::bail!("gateway has IPv6 address {ipv6}, expected IPv4"),
    }
}

fn detect_local_addr() -> Result<Ipv4Addr> {
    default_net::get_interfaces()
        .iter()
        .filter(|iface| !iface.is_loopback())
        .flat_map(|iface| iface.ipv4.iter().map(|net| net.addr))
        .find(|addr| !addr.is_loopback() && !addr.is_link_local())
        .context("no non-loopback, non-link-local IPv4 address found")
}

async fn try_upnp(internal_port: u16, local_ip: Ipv4Addr) -> Result<PortMapping> {
    let gw = igd_next::aio::tokio::search_gateway(igd_next::SearchOptions {
        timeout: Some(PORT_MAPPING_TIMEOUT),
        ..Default::default()
    })
    .await
    .map_err(|e| anyhow::anyhow!("UPnP gateway search failed: {e}"))?;

    let ext_ip = gw
        .get_external_ip()
        .await
        .map_err(|e| anyhow::anyhow!("UPnP get external IP failed: {e}"))?;

    gw.add_port(
        igd_next::PortMappingProtocol::UDP,
        internal_port,
        SocketAddr::V4(SocketAddrV4::new(local_ip, internal_port)),
        0,
        "punchgate",
    )
    .await
    .map_err(|e| anyhow::anyhow!("UPnP add port failed: {e}"))?;

    Ok(PortMapping {
        external_addr: SocketAddr::new(ext_ip, internal_port),
        protocol_used: "UPnP",
    })
}

async fn try_nat_pmp_pcp(
    internal_port: u16,
    gateway_ip: Ipv4Addr,
    local_ip: Ipv4Addr,
) -> Result<PortMapping> {
    let port = NonZeroU16::new(internal_port).context("port must be non-zero")?;
    let gateway_addr = IpAddr::V4(gateway_ip);
    let client_addr = IpAddr::V4(local_ip);

    let mapping = crab_nat::PortMapping::new(
        gateway_addr,
        client_addr,
        crab_nat::InternetProtocol::Udp,
        port,
        crab_nat::PortMappingOptions::default(),
    )
    .await
    .map_err(|e| anyhow::anyhow!("NAT-PMP/PCP failed: {e:?}"))?;

    let external_port = mapping.external_port().get();

    let (external_ip, protocol_used) = match mapping.mapping_type() {
        crab_nat::PortMappingType::Pcp { external_ip, .. } => (external_ip, "PCP"),
        crab_nat::PortMappingType::NatPmp => {
            let ext_ipv4 = crab_nat::natpmp::external_address(gateway_addr, None)
                .await
                .map_err(|e| anyhow::anyhow!("NAT-PMP external address query failed: {e:?}"))?;
            (IpAddr::V4(ext_ipv4), "NAT-PMP")
        }
    };

    Ok(PortMapping {
        external_addr: SocketAddr::new(external_ip, external_port),
        protocol_used,
    })
}

pub async fn acquire_port_mapping(internal_port: u16) -> Option<PortMapping> {
    let (gateway_ip, local_ip) = match detect_gateway().and_then(|gw| {
        let local = detect_local_addr()?;
        Ok((gw, local))
    }) {
        Ok(pair) => pair,
        Err(e) => {
            warn!("port mapping: network detection failed: {e:#}");
            return None;
        }
    };

    info!(
        %gateway_ip,
        %local_ip,
        "port mapping: detected gateway and local address"
    );

    let result = tokio::time::timeout(PORT_MAPPING_TIMEOUT, async {
        let upnp = try_upnp(internal_port, local_ip);
        let nat_pmp = try_nat_pmp_pcp(internal_port, gateway_ip, local_ip);

        tokio::pin!(upnp);
        tokio::pin!(nat_pmp);

        let mut upnp_done = false;
        let mut nat_pmp_done = false;

        loop {
            tokio::select! {
                result = &mut upnp, if !upnp_done => {
                    match result {
                        Ok(mapping) => {
                            info!(
                                protocol = mapping.protocol_used,
                                %mapping.external_addr,
                                "port mapping: succeeded"
                            );
                            return Some(mapping);
                        }
                        Err(e) => {
                            warn!("port mapping: UPnP failed: {e:#}");
                            upnp_done = true;
                        }
                    }
                }
                result = &mut nat_pmp, if !nat_pmp_done => {
                    match result {
                        Ok(mapping) => {
                            info!(
                                protocol = mapping.protocol_used,
                                %mapping.external_addr,
                                "port mapping: succeeded"
                            );
                            return Some(mapping);
                        }
                        Err(e) => {
                            warn!("port mapping: NAT-PMP/PCP failed: {e:#}");
                            nat_pmp_done = true;
                        }
                    }
                }
            }

            if upnp_done && nat_pmp_done {
                return None;
            }
        }
    })
    .await;

    match result {
        Ok(mapping) => mapping,
        Err(_) => {
            warn!("port mapping: all probes timed out");
            None
        }
    }
}
