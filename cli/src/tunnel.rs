use std::{collections::HashMap, net::SocketAddr, sync::Arc, time::Duration};

use anyhow::{Context, Result, bail};
use futures::io::{AsyncReadExt as FuturesAsyncReadExt, AsyncWriteExt as FuturesAsyncWriteExt};
use libp2p::{PeerId, Stream as LibP2pStream};
use libp2p_stream::IncomingStreams;
use serde::{Deserialize, Serialize};
use tokio::{
    io::copy_bidirectional,
    net::{TcpListener, TcpStream},
    task::JoinHandle,
};
use tokio_util::compat::FuturesAsyncReadCompatExt;

use crate::protocol;

// ─── Wire format ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TunnelRequest {
    pub service_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TunnelResponse {
    pub accepted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Write a length-prefixed JSON message to a futures `AsyncWrite` stream.
async fn write_message<T: Serialize>(
    stream: &mut (impl futures::io::AsyncWrite + Unpin),
    msg: &T,
) -> Result<()> {
    let data = serde_json::to_vec(msg)?;
    let len = u32::try_from(data.len()).context("message too large for u32 length prefix")?;
    stream.write_all(&len.to_be_bytes()).await?;
    stream.write_all(&data).await?;
    stream.flush().await?;
    Ok(())
}

/// Read a length-prefixed JSON message from a futures `AsyncRead` stream.
async fn read_message<T: for<'de> Deserialize<'de>>(
    stream: &mut (impl futures::io::AsyncRead + Unpin),
) -> Result<T> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await?;
    // Infallible: u32 always fits in usize on all Rust std-supported platforms (>= 32-bit)
    let len = usize::try_from(u32::from_be_bytes(len_buf))
        .expect("u32 fits in usize on all supported platforms");
    if len > 1024 * 64 {
        bail!("message too large: {len} bytes");
    }
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf).await?;
    Ok(serde_json::from_slice(&buf)?)
}

// ─── Server side: accept loop ───────────────────────────────────────────────

/// Accept incoming tunnel streams and proxy them to local services.
pub async fn accept_loop(
    mut incoming: IncomingStreams,
    services: Arc<HashMap<String, SocketAddr>>,
) {
    use futures::StreamExt;

    while let Some((peer_id, stream)) = incoming.next().await {
        let services = services.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_tunnel(peer_id, stream, &services).await {
                tracing::warn!(%peer_id, error = %e, "tunnel handler failed");
            }
        });
    }
}

async fn handle_tunnel(
    peer_id: PeerId,
    mut stream: LibP2pStream,
    services: &HashMap<String, SocketAddr>,
) -> Result<()> {
    let request: TunnelRequest = read_message(&mut stream).await?;
    tracing::info!(%peer_id, service = %request.service_name, "tunnel request");

    let Some(&local_addr) = services.get(&request.service_name) else {
        write_message(
            &mut stream,
            &TunnelResponse {
                accepted: false,
                reason: Some(format!("unknown service: {}", request.service_name)),
            },
        )
        .await?;
        return Ok(());
    };

    // Connect to the local service
    let tcp_stream = TcpStream::connect(local_addr)
        .await
        .with_context(|| format!("connecting to local service at {local_addr}"))?;

    write_message(
        &mut stream,
        &TunnelResponse {
            accepted: true,
            reason: None,
        },
    )
    .await?;

    // Bridge: libp2p stream (futures) ↔ TCP stream (tokio)
    let mut compat_stream = stream.compat();
    let mut tcp_stream = tcp_stream;

    match copy_bidirectional(&mut compat_stream, &mut tcp_stream).await {
        Ok((to_tcp, from_tcp)) => {
            tracing::debug!(
                %peer_id,
                to_tcp,
                from_tcp,
                "tunnel closed normally"
            );
        }
        Err(e) => {
            tracing::debug!(%peer_id, error = %e, "tunnel I/O error");
        }
    }

    Ok(())
}

// ─── Client side: connect tunnel ────────────────────────────────────────────

/// Connect to a remote service via a libp2p tunnel and expose it on a local TCP port.
pub async fn connect_tunnel(
    mut control: libp2p_stream::Control,
    remote_peer: PeerId,
    service_name: String,
    bind_addr: SocketAddr,
) -> Result<()> {
    let listener = TcpListener::bind(bind_addr)
        .await
        .with_context(|| format!("binding to {bind_addr}"))?;

    let actual_addr = listener.local_addr()?;
    tracing::info!(
        %remote_peer,
        %service_name,
        %actual_addr,
        "tunnel listening"
    );

    loop {
        let (tcp_stream, client_addr) = listener.accept().await?;
        tracing::debug!(%client_addr, "new tunnel client");

        let service_name = service_name.clone();
        let mut stream = control
            .open_stream(remote_peer, protocol::tunnel_protocol())
            .await
            .with_context(|| format!("opening stream to {remote_peer}"))?;

        tokio::spawn(async move {
            if let Err(e) = client_tunnel_session(&mut stream, tcp_stream, &service_name).await {
                tracing::warn!(
                    %client_addr,
                    error = %e,
                    "tunnel session failed"
                );
            }
        });
    }
}

async fn client_tunnel_session(
    stream: &mut LibP2pStream,
    mut tcp_stream: TcpStream,
    service_name: &str,
) -> Result<()> {
    // Send request
    write_message(
        stream,
        &TunnelRequest {
            service_name: service_name.to_string(),
        },
    )
    .await?;

    // Read response
    let response: TunnelResponse = read_message(stream).await?;

    if !response.accepted {
        bail!("tunnel rejected: {}", response.reason.unwrap_or_default());
    }

    // Bridge
    let mut compat_stream = stream.compat();
    copy_bidirectional(&mut compat_stream, &mut tcp_stream).await?;

    Ok(())
}

// ─── Tunnel spec parsing ────────────────────────────────────────────────────

/// Parse a `--tunnel` spec: `peer_id:service_name@bind_addr`
pub fn parse_tunnel_spec(spec: &str) -> Result<(PeerId, String, SocketAddr)> {
    let (peer_and_service, bind_str) = spec
        .rsplit_once('@')
        .context("tunnel format: peer_id:service_name@bind_addr")?;

    let (peer_str, service_name) = peer_and_service
        .split_once(':')
        .context("tunnel format: peer_id:service_name@bind_addr")?;

    let peer_id: PeerId = peer_str
        .parse()
        .with_context(|| format!("invalid peer ID: {peer_str}"))?;

    let bind_addr: SocketAddr = bind_str
        .parse()
        .with_context(|| format!("invalid bind address: {bind_str}"))?;

    Ok((peer_id, service_name.to_string(), bind_addr))
}

// ─── Tunnel registry ────────────────────────────────────────────────────────

/// Tracks active tunnel tasks for graceful shutdown.
pub struct TunnelRegistry {
    tunnels: HashMap<u64, (String, JoinHandle<()>)>,
    next_id: u64,
}

impl Default for TunnelRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl TunnelRegistry {
    pub fn new() -> Self {
        Self {
            tunnels: HashMap::new(),
            next_id: 0,
        }
    }

    /// Register a tunnel task. Returns its ID.
    pub fn register(&mut self, label: String, handle: JoinHandle<()>) -> u64 {
        let id = self.next_id;
        // Infallible: u64 overflow requires 2^64 registrations, physically impossible
        self.next_id = self
            .next_id
            .checked_add(1)
            .expect("u64 overflow requires 2^64 registrations");
        self.tunnels.insert(id, (label, handle));
        id
    }

    /// Remove a completed tunnel.
    pub fn unregister(&mut self, id: u64) {
        self.tunnels.remove(&id);
    }

    /// Abort all tunnel tasks with a timeout for graceful close.
    pub async fn shutdown_all(&mut self, timeout: Duration) {
        let count = self.tunnels.len();
        if count == 0 {
            return;
        }
        tracing::info!(count, "shutting down tunnel tasks");

        for (_, (label, handle)) in self.tunnels.drain() {
            handle.abort();
            match tokio::time::timeout(timeout, handle).await {
                Ok(_) => tracing::debug!(%label, "tunnel shut down"),
                Err(_) => tracing::warn!(%label, "tunnel shutdown timed out"),
            }
        }
    }

    pub fn active_count(&self) -> usize {
        self.tunnels.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tunnel_request_roundtrip() {
        let req = TunnelRequest {
            service_name: "echo".into(),
        };
        let data = serde_json::to_vec(&req).expect("serialize tunnel request");
        let decoded: TunnelRequest =
            serde_json::from_slice(&data).expect("deserialize tunnel request");
        assert_eq!(decoded.service_name, "echo");
    }

    #[test]
    fn tunnel_response_roundtrip() {
        let resp = TunnelResponse {
            accepted: true,
            reason: None,
        };
        let data = serde_json::to_vec(&resp).expect("serialize accepted response");
        let decoded: TunnelResponse =
            serde_json::from_slice(&data).expect("deserialize accepted response");
        assert!(decoded.accepted);
        assert!(decoded.reason.is_none());

        let resp = TunnelResponse {
            accepted: false,
            reason: Some("not found".into()),
        };
        let data = serde_json::to_vec(&resp).expect("serialize rejected response");
        let decoded: TunnelResponse =
            serde_json::from_slice(&data).expect("deserialize rejected response");
        assert!(!decoded.accepted);
        assert_eq!(
            decoded.reason.expect("rejection reason present"),
            "not found"
        );
    }

    #[test]
    fn parse_tunnel_spec_valid() {
        let peer_id = PeerId::random();
        let spec = format!("{peer_id}:echo@127.0.0.1:9000");
        let (pid, name, addr) = parse_tunnel_spec(&spec).expect("parse valid tunnel spec");
        assert_eq!(pid, peer_id);
        assert_eq!(name, "echo");
        assert_eq!(
            addr,
            "127.0.0.1:9000"
                .parse::<SocketAddr>()
                .expect("parse socket address")
        );
    }

    #[test]
    fn parse_tunnel_spec_invalid() {
        assert!(parse_tunnel_spec("garbage").is_err());
        assert!(parse_tunnel_spec("peer:svc").is_err()); // missing @
    }
}
