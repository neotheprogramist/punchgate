use std::net::SocketAddr;

use libp2p::{Multiaddr, multiaddr::Protocol};
use tokio::net::UdpSocket;
use tracing::{debug, warn};

const PRIME_PACKET: &[u8] = b"punch";
const PRIME_COUNT: usize = 3;

pub fn extract_udp_socket_addr(addr: &Multiaddr) -> Option<SocketAddr> {
    let mut ip = None;
    let mut port = None;
    for proto in addr.iter() {
        match proto {
            Protocol::Ip4(a) => ip = Some(std::net::IpAddr::V4(a)),
            Protocol::Ip6(a) => ip = Some(std::net::IpAddr::V6(a)),
            Protocol::Udp(p) => port = Some(p),
            _ => {}
        }
    }
    ip.zip(port).map(|(ip, port)| SocketAddr::new(ip, port))
}

pub async fn send_nat_priming(local_listen_addr: SocketAddr, peer_addrs: &[Multiaddr]) {
    let targets: Vec<SocketAddr> = peer_addrs
        .iter()
        .filter(|a| !a.iter().any(|p| matches!(p, Protocol::P2pCircuit)))
        .filter_map(extract_udp_socket_addr)
        .filter(|sa| !sa.ip().is_loopback() && !sa.ip().is_unspecified())
        .collect();

    if targets.is_empty() {
        debug!("NAT priming: no direct peer addresses to prime");
        return;
    }

    let std_socket = {
        let domain = match local_listen_addr {
            SocketAddr::V4(_) => socket2::Domain::IPV4,
            SocketAddr::V6(_) => socket2::Domain::IPV6,
        };
        let socket = match socket2::Socket::new(
            domain,
            socket2::Type::DGRAM,
            Some(socket2::Protocol::UDP),
        ) {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "NAT priming: failed to create socket");
                return;
            }
        };
        if let Err(e) = socket.set_reuse_port(true) {
            warn!(error = %e, "NAT priming: failed to set SO_REUSEPORT");
            return;
        }
        if let Err(e) = socket.set_reuse_address(true) {
            warn!(error = %e, "NAT priming: failed to set SO_REUSEADDR");
            return;
        }
        if let Err(e) = socket.bind(&local_listen_addr.into()) {
            warn!(addr = %local_listen_addr, error = %e, "NAT priming: failed to bind");
            return;
        }
        socket.set_nonblocking(true).ok();
        socket
    };

    let socket = match UdpSocket::from_std(std_socket.into()) {
        Ok(s) => s,
        Err(e) => {
            warn!(error = %e, "NAT priming: failed to convert to tokio socket");
            return;
        }
    };

    for target in &targets {
        for _ in 0..PRIME_COUNT {
            match socket.send_to(PRIME_PACKET, target).await {
                Ok(_) => {}
                Err(e) => {
                    debug!(target = %target, error = %e, "NAT priming: send failed");
                    break;
                }
            }
        }
        debug!(target = %target, count = PRIME_COUNT, "NAT priming: sent packets");
    }
}
