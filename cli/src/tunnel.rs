use std::{collections::HashMap, sync::Arc, time::Duration};

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

use crate::{protocol, specs::ServiceAddr, types::ServiceName};

// ─── Wire format ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TunnelRequest {
    pub service_name: ServiceName,
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
    const MAX_MESSAGE_SIZE: usize = 65536;
    match len {
        0..=MAX_MESSAGE_SIZE => {}
        _ => bail!("message too large: {len} bytes"),
    }
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf).await?;
    Ok(serde_json::from_slice(&buf)?)
}

// ─── Server side: accept loop ───────────────────────────────────────────────

pub async fn accept_loop(
    mut incoming: IncomingStreams,
    services: Arc<HashMap<ServiceName, ServiceAddr>>,
) {
    use futures::StreamExt;

    while let Some((peer_id, stream)) = incoming.next().await {
        let services = services.clone();
        tokio::spawn(async move {
            match handle_tunnel(peer_id, stream, &services).await {
                Ok(()) => {}
                Err(e) => tracing::warn!(%peer_id, error = %e, "tunnel handler failed"),
            }
        });
    }
}

async fn handle_tunnel(
    peer_id: PeerId,
    mut stream: LibP2pStream,
    services: &HashMap<ServiceName, ServiceAddr>,
) -> Result<()> {
    let request: TunnelRequest = read_message(&mut stream).await?;
    tracing::info!(%peer_id, service = %request.service_name, "tunnel request");

    let local_addr = match services.get(&request.service_name) {
        Some(addr) => addr,
        None => {
            tracing::warn!(%peer_id, service = %request.service_name, reason = "unknown service", "tunnel rejected");
            write_message(
                &mut stream,
                &TunnelResponse {
                    accepted: false,
                    reason: Some(format!("unknown service: {}", request.service_name)),
                },
            )
            .await?;
            return Ok(());
        }
    };

    let tcp_stream = TcpStream::connect(local_addr.connect_tuple())
        .await
        .with_context(|| format!("connecting to local service at {local_addr}"))?;

    let service_name = &request.service_name;

    write_message(
        &mut stream,
        &TunnelResponse {
            accepted: true,
            reason: None,
        },
    )
    .await?;

    tracing::info!(%peer_id, service = %service_name, %local_addr, "tunnel accepted");

    // Bridge: libp2p stream (futures) ↔ TCP stream (tokio)
    let mut compat_stream = stream.compat();
    let mut tcp_stream = tcp_stream;

    match copy_bidirectional(&mut compat_stream, &mut tcp_stream).await {
        Ok((bytes_to_service, bytes_from_service)) => {
            tracing::info!(
                %peer_id,
                service = %service_name,
                bytes_to_service,
                bytes_from_service,
                "tunnel closed"
            );
        }
        Err(e) => {
            tracing::warn!(%peer_id, error = %e, "tunnel error");
        }
    }

    Ok(())
}

// ─── Client side: connect tunnel ────────────────────────────────────────────

pub async fn connect_tunnel(
    mut control: libp2p_stream::Control,
    remote_peer: PeerId,
    service_name: ServiceName,
    bind_addr: ServiceAddr,
) -> Result<()> {
    let listener = TcpListener::bind(bind_addr.connect_tuple())
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
            match client_tunnel_session(&mut stream, tcp_stream, &service_name).await {
                Ok(()) => {}
                Err(e) => tracing::warn!(
                    %client_addr,
                    error = %e,
                    "tunnel session failed"
                ),
            }
        });
    }
}

async fn client_tunnel_session(
    stream: &mut LibP2pStream,
    mut tcp_stream: TcpStream,
    service_name: &ServiceName,
) -> Result<()> {
    write_message(
        stream,
        &TunnelRequest {
            service_name: service_name.clone(),
        },
    )
    .await?;

    let response: TunnelResponse = read_message(stream).await?;

    match response.accepted {
        true => {}
        false => {
            let reason = response.reason.as_deref().unwrap_or("unknown");
            tracing::warn!(service = %service_name, %reason, "client tunnel rejected");
            bail!("tunnel rejected: {reason}");
        }
    }

    let mut compat_stream = stream.compat();
    match copy_bidirectional(&mut compat_stream, &mut tcp_stream).await {
        Ok((bytes_to_remote, bytes_from_remote)) => {
            tracing::info!(service = %service_name, bytes_to_remote, bytes_from_remote, "client tunnel closed");
        }
        Err(e) => {
            tracing::warn!(service = %service_name, error = %e, "client tunnel error");
            return Err(e.into());
        }
    }

    Ok(())
}

// ─── Tunnel registry ────────────────────────────────────────────────────────

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

    pub fn unregister(&mut self, id: u64) {
        self.tunnels.remove(&id);
    }

    pub async fn shutdown_all(&mut self, timeout: Duration) {
        match self.tunnels.len() {
            0 => return,
            count => tracing::info!(count, "shutting down tunnel tasks"),
        }

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
