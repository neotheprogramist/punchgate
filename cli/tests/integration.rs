use std::{collections::HashMap, net::SocketAddr, sync::Arc, time::Duration};

use futures::StreamExt;
#[cfg(feature = "mdns")]
use libp2p::mdns;
use libp2p::{PeerId, SwarmBuilder, identify, kad, noise, swarm::SwarmEvent, yamux};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::oneshot,
};

// ─── Helper: build a minimal swarm for testing ──────────────────────────────

fn build_test_swarm() -> (libp2p::Swarm<TestBehaviour>, PeerId, libp2p_stream::Control) {
    let keypair = libp2p::identity::Keypair::generate_ed25519();
    let peer_id = PeerId::from(keypair.public());

    let mut swarm = SwarmBuilder::with_existing_identity(keypair.clone())
        .with_tokio()
        .with_tcp(
            Default::default(),
            noise::Config::new,
            yamux::Config::default,
        )
        .expect("build TCP transport")
        .with_behaviour(|key| {
            let peer_id = PeerId::from(key.public());

            let kad_config = kad::Config::new(cli::protocol::kad_protocol());
            let store = kad::store::MemoryStore::new(peer_id);
            let kademlia = kad::Behaviour::with_config(peer_id, store, kad_config);

            #[cfg(feature = "mdns")]
            let mdns = mdns::tokio::Behaviour::new(mdns::Config::default(), peer_id)
                .expect("create mDNS behaviour");

            let identify = identify::Behaviour::new(identify::Config::new(
                cli::protocol::IDENTIFY_PROTOCOL.to_string(),
                key.public(),
            ));

            let stream = libp2p_stream::Behaviour::new();

            TestBehaviour {
                kademlia,
                #[cfg(feature = "mdns")]
                mdns,
                identify,
                stream,
            }
        })
        .expect("build swarm behaviour")
        .with_swarm_config(|cfg| cfg.with_idle_connection_timeout(Duration::from_secs(30)))
        .build();

    let control = swarm.behaviour().stream.new_control();
    swarm
        .listen_on(
            "/ip4/127.0.0.1/tcp/0"
                .parse()
                .expect("parse listen multiaddr"),
        )
        .expect("start listening");

    (swarm, peer_id, control)
}

#[derive(libp2p::swarm::NetworkBehaviour)]
struct TestBehaviour {
    kademlia: kad::Behaviour<kad::store::MemoryStore>,
    #[cfg(feature = "mdns")]
    mdns: mdns::tokio::Behaviour,
    identify: identify::Behaviour,
    stream: libp2p_stream::Behaviour,
}

// ─── Test 1: Two-peer mDNS discovery ────────────────────────────────────────

#[cfg(feature = "mdns")]
#[tokio::test]
async fn two_peer_mdns_discovery() {
    let (mut swarm_a, _peer_a, _) = build_test_swarm();
    let (mut swarm_b, peer_b, _) = build_test_swarm();

    let (tx_a, rx_a) = oneshot::channel();
    let (tx_b, rx_b) = oneshot::channel();
    let (tx_discovered, rx_discovered) = oneshot::channel::<PeerId>();

    // Run swarm A
    let handle_a = tokio::spawn(async move {
        let mut tx_a = Some(tx_a);
        let mut tx_discovered = Some(tx_discovered);
        loop {
            let event = swarm_a.select_next_some().await;
            match event {
                SwarmEvent::NewListenAddr { address, .. } => {
                    if let Some(tx) = tx_a.take() {
                        let _ = tx.send(address);
                    }
                }
                SwarmEvent::Behaviour(TestBehaviourEvent::Mdns(mdns::Event::Discovered(peers))) => {
                    for (pid, _) in &peers {
                        if *pid == peer_b {
                            if let Some(tx) = tx_discovered.take() {
                                let _ = tx.send(*pid);
                            }
                            return;
                        }
                    }
                }
                // SwarmEvent is #[non_exhaustive] — wildcard required
                _ => {}
            }
        }
    });

    // Run swarm B
    let handle_b = tokio::spawn(async move {
        let mut tx_b = Some(tx_b);
        loop {
            let event = swarm_b.select_next_some().await;
            if let SwarmEvent::NewListenAddr { address, .. } = event
                && let Some(tx) = tx_b.take()
            {
                let _ = tx.send(address);
            }
        }
    });

    // Wait for both to start listening
    let _addr_a = tokio::time::timeout(Duration::from_secs(5), rx_a)
        .await
        .expect("swarm A timed out listening")
        .expect("swarm A addr channel closed");

    let _addr_b = tokio::time::timeout(Duration::from_secs(5), rx_b)
        .await
        .expect("swarm B timed out listening")
        .expect("swarm B addr channel closed");

    // Wait for A to discover B via mDNS
    let discovered_peer = tokio::time::timeout(Duration::from_secs(30), rx_discovered)
        .await
        .expect("mDNS discovery timed out")
        .expect("discovery channel closed");

    assert_eq!(discovered_peer, peer_b);

    handle_a.abort();
    handle_b.abort();
}

// ─── Test 2: Three-peer tunnel test ─────────────────────────────────────────

#[tokio::test]
async fn three_peer_tunnel() {
    // Peer A: runs for connectivity
    // Peer B: exposes an echo service
    // Peer C: connects a tunnel to B's echo service

    let (mut swarm_a, peer_a, _control_a) = build_test_swarm();
    let (mut swarm_b, peer_b, mut control_b) = build_test_swarm();
    let (mut swarm_c, _peer_c, mut control_c) = build_test_swarm();

    let (tx_a_addr, rx_a_addr) = oneshot::channel();
    let (tx_b_addr, rx_b_addr) = oneshot::channel();
    let (tx_b_discovered_a, rx_b_discovered_a) = oneshot::channel::<()>();

    // Start local echo server for Peer B to proxy to
    let echo_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind echo listener");
    let echo_addr = echo_listener
        .local_addr()
        .expect("get echo listener address");

    let echo_handle = tokio::spawn(async move {
        while let Ok((mut stream, _)) = echo_listener.accept().await {
            tokio::spawn(async move {
                let mut buf = [0u8; 1024];
                loop {
                    match stream.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if stream.write_all(&buf[..n]).await.is_err() {
                                break;
                            }
                        }
                    }
                }
            });
        }
    });

    // Register tunnel accept on Peer B
    let services: Arc<HashMap<String, SocketAddr>> =
        Arc::new([("echo".to_string(), echo_addr)].into());

    let incoming_b = control_b
        .accept(cli::protocol::tunnel_protocol())
        .expect("register tunnel protocol");
    tokio::spawn(cli::tunnel::accept_loop(incoming_b, services));

    // Run Peer A
    let handle_a = tokio::spawn(async move {
        let mut tx = Some(tx_a_addr);
        loop {
            let event = swarm_a.select_next_some().await;
            if let SwarmEvent::NewListenAddr { address, .. } = event
                && let Some(tx) = tx.take()
            {
                let _ = tx.send(address);
            }
        }
    });

    // Get A's listen address
    let addr_a = tokio::time::timeout(Duration::from_secs(5), rx_a_addr)
        .await
        .expect("A addr timeout")
        .expect("A addr closed");

    // Peer B: dial A, then announce listening address
    let addr_a_for_b = addr_a.clone();
    let handle_b = tokio::spawn(async move {
        swarm_b.dial(addr_a_for_b).expect("B dial A");
        let mut tx_addr = Some(tx_b_addr);
        let mut tx_disc = Some(tx_b_discovered_a);

        loop {
            let event = swarm_b.select_next_some().await;
            match event {
                SwarmEvent::NewListenAddr { address, .. } => {
                    if let Some(tx) = tx_addr.take() {
                        let _ = tx.send(address);
                    }
                }
                SwarmEvent::ConnectionEstablished { peer_id, .. } if peer_id == peer_a => {
                    if let Some(tx) = tx_disc.take() {
                        let _ = tx.send(());
                    }
                }
                // SwarmEvent is #[non_exhaustive] — wildcard required
                _ => {}
            }
        }
    });

    // Wait for B to discover A
    tokio::time::timeout(Duration::from_secs(10), rx_b_discovered_a)
        .await
        .expect("B discovering A timed out")
        .expect("B discovering A channel closed");

    // Get B's listen address
    let addr_b = tokio::time::timeout(Duration::from_secs(5), rx_b_addr)
        .await
        .expect("B addr timeout")
        .expect("B addr closed");

    // Peer C: dial B directly
    let addr_b_with_peer = addr_b
        .clone()
        .with(libp2p::multiaddr::Protocol::P2p(peer_b));

    let (tx_c_connected, rx_c_connected) = oneshot::channel::<()>();

    let handle_c = tokio::spawn(async move {
        swarm_c.dial(addr_b_with_peer).expect("C dial B");
        let mut tx = Some(tx_c_connected);
        loop {
            let event = swarm_c.select_next_some().await;
            if let SwarmEvent::ConnectionEstablished { peer_id, .. } = event
                && peer_id == peer_b
                && let Some(tx) = tx.take()
            {
                let _ = tx.send(());
            }
        }
    });

    // Wait for C to connect to B
    tokio::time::timeout(Duration::from_secs(10), rx_c_connected)
        .await
        .expect("C connecting to B timed out")
        .expect("C connection channel closed");

    // Now C opens a tunnel stream to B
    let mut stream = control_c
        .open_stream(peer_b, cli::protocol::tunnel_protocol())
        .await
        .expect("open tunnel stream");

    // Send tunnel request (length-prefixed JSON)
    use futures::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let req = serde_json::to_vec(&cli::tunnel::TunnelRequest {
        service_name: "echo".into(),
    })
    .expect("serialize tunnel request");
    let len = u32::try_from(req.len())
        .expect("request length fits in u32")
        .to_be_bytes();
    stream.write_all(&len).await.expect("write request length");
    stream.write_all(&req).await.expect("write request body");
    stream.flush().await.expect("flush request");

    // Read response
    let mut len_buf = [0u8; 4];
    stream
        .read_exact(&mut len_buf)
        .await
        .expect("read response length");
    // Infallible: u32 always fits in usize on all Rust std-supported platforms (>= 32-bit)
    let resp_len = usize::try_from(u32::from_be_bytes(len_buf))
        .expect("u32 fits in usize on all supported platforms");
    let mut resp_buf = vec![0u8; resp_len];
    stream
        .read_exact(&mut resp_buf)
        .await
        .expect("read response body");
    let resp: cli::tunnel::TunnelResponse =
        serde_json::from_slice(&resp_buf).expect("deserialize tunnel response");
    assert!(resp.accepted, "tunnel should be accepted");

    // Send test data through the tunnel
    let test_data = b"hello punchgate!";
    stream.write_all(test_data).await.expect("write test data");
    stream.flush().await.expect("flush test data");

    // Read echo back
    let mut echo_buf = vec![0u8; test_data.len()];
    stream
        .read_exact(&mut echo_buf)
        .await
        .expect("read echo response");
    assert_eq!(&echo_buf, test_data, "echo data should match");

    // Clean up
    handle_a.abort();
    handle_b.abort();
    handle_c.abort();
    echo_handle.abort();
}
