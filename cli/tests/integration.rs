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
            let mut kademlia = kad::Behaviour::with_config(peer_id, store, kad_config);
            kademlia.set_mode(Some(kad::Mode::Server));

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

// ─── Test 2: Service discovery via DHT GetProviders ─────────────────────────

#[tokio::test]
async fn service_discovery_tunnel() {
    // Peer A: bootstrap/DHT backbone
    // Peer B: advertises "echo" service via DHT, runs echo server
    // Peer C: queries GetProviders for "echo", discovers B, opens tunnel

    let (mut swarm_a, peer_a, _control_a) = build_test_swarm();
    let (mut swarm_b, peer_b, mut control_b) = build_test_swarm();
    let (mut swarm_c, _peer_c, mut control_c) = build_test_swarm();

    let (tx_a_addr, rx_a_addr) = oneshot::channel();
    let (tx_b_addr, rx_b_addr) = oneshot::channel();

    // Start local echo server for Peer B
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

    // Run Peer A (bootstrap) — must handle Identify to populate Kademlia routing table
    let handle_a = tokio::spawn(async move {
        let mut tx = Some(tx_a_addr);
        loop {
            let event = swarm_a.select_next_some().await;
            match event {
                SwarmEvent::NewListenAddr { address, .. } => {
                    if let Some(tx) = tx.take() {
                        let _ = tx.send(address);
                    }
                }
                SwarmEvent::Behaviour(TestBehaviourEvent::Identify(
                    identify::Event::Received { peer_id, info, .. },
                )) => {
                    for addr in &info.listen_addrs {
                        swarm_a
                            .behaviour_mut()
                            .kademlia
                            .add_address(&peer_id, addr.clone());
                    }
                }
                _ => {}
            }
        }
    });

    // Get A's listen address
    let addr_a = tokio::time::timeout(Duration::from_secs(5), rx_a_addr)
        .await
        .expect("A addr timeout")
        .expect("A addr closed");

    // Peer B: dial A, advertise "echo" in DHT, then loop
    let addr_a_for_b = addr_a.clone();
    let (tx_b_providing, rx_b_providing) = oneshot::channel::<()>();
    let handle_b = tokio::spawn(async move {
        // Add A to Kademlia routing table and dial
        swarm_b
            .behaviour_mut()
            .kademlia
            .add_address(&peer_a, addr_a_for_b.clone());
        swarm_b.dial(addr_a_for_b).expect("B dial A");

        let mut tx_addr = Some(tx_b_addr);
        let mut tx_prov = Some(tx_b_providing);
        let mut bootstrapped = false;
        let mut connected_to_a = false;

        loop {
            let event = swarm_b.select_next_some().await;
            match event {
                SwarmEvent::NewListenAddr { address, .. } => {
                    if let Some(tx) = tx_addr.take() {
                        let _ = tx.send(address);
                    }
                }
                SwarmEvent::ConnectionEstablished { peer_id, .. } if peer_id == peer_a => {
                    connected_to_a = true;
                    if !bootstrapped {
                        let _ = swarm_b.behaviour_mut().kademlia.bootstrap();
                        bootstrapped = true;
                    }
                }
                SwarmEvent::Behaviour(TestBehaviourEvent::Kademlia(
                    kad::Event::OutboundQueryProgressed {
                        result: kad::QueryResult::Bootstrap(Ok(_)),
                        ..
                    },
                )) if connected_to_a => {
                    // Advertise "echo" service in DHT
                    let key = cli::service::service_key("echo");
                    let record_key = kad::RecordKey::new(&key);
                    let _ = swarm_b.behaviour_mut().kademlia.start_providing(record_key);
                }
                SwarmEvent::Behaviour(TestBehaviourEvent::Kademlia(
                    kad::Event::OutboundQueryProgressed {
                        result: kad::QueryResult::StartProviding(Ok(_)),
                        ..
                    },
                )) => {
                    if let Some(tx) = tx_prov.take() {
                        let _ = tx.send(());
                    }
                }
                _ => {}
            }
        }
    });

    // Wait for B to finish advertising
    let addr_b = tokio::time::timeout(Duration::from_secs(5), rx_b_addr)
        .await
        .expect("B addr timeout")
        .expect("B addr closed");

    tokio::time::timeout(Duration::from_secs(30), rx_b_providing)
        .await
        .expect("B providing timed out")
        .expect("B providing channel closed");

    // Peer C: dial A, bootstrap, query GetProviders for "echo", discover B,
    // dial B, then open tunnel — entire flow runs inside the spawned task
    let addr_a_for_c = addr_a.clone();
    let (tx_c_result, rx_c_result) = oneshot::channel::<(PeerId, Vec<u8>)>();
    let handle_c = tokio::spawn(async move {
        swarm_c
            .behaviour_mut()
            .kademlia
            .add_address(&peer_a, addr_a_for_c.clone());
        swarm_c.dial(addr_a_for_c).expect("C dial A");

        let mut bootstrapped = false;
        let mut queried_providers = false;
        let mut discovered_provider: Option<PeerId> = None;
        let mut connected_to_provider = false;
        let mut tx_result = Some(tx_c_result);
        let mut retry_interval = tokio::time::interval(Duration::from_secs(2));
        retry_interval.tick().await; // skip first immediate tick

        loop {
            tokio::select! {
                event = swarm_c.select_next_some() => {
                    match event {
                        SwarmEvent::ConnectionEstablished { peer_id, .. } if peer_id == peer_a => {
                            if !bootstrapped {
                                let _ = swarm_c.behaviour_mut().kademlia.bootstrap();
                                bootstrapped = true;
                            }
                        }
                        SwarmEvent::Behaviour(TestBehaviourEvent::Kademlia(
                            kad::Event::OutboundQueryProgressed {
                                result: kad::QueryResult::Bootstrap(Ok(_)),
                                ..
                            },
                        )) if !queried_providers => {
                            queried_providers = true;
                            let key = cli::service::service_key("echo");
                            let record_key = kad::RecordKey::new(&key);
                            swarm_c.behaviour_mut().kademlia.get_providers(record_key);
                        }
                        SwarmEvent::Behaviour(TestBehaviourEvent::Kademlia(
                            kad::Event::OutboundQueryProgressed {
                                result:
                                    kad::QueryResult::GetProviders(Ok(
                                        kad::GetProvidersOk::FoundProviders { providers, .. },
                                    )),
                                ..
                            },
                        )) if discovered_provider.is_none() => {
                            if let Some(&provider) = providers.iter().next() {
                                discovered_provider = Some(provider);
                                // Kademlia may have already connected to B during queries
                                if !swarm_c.is_connected(&provider) {
                                    let provider_addr = addr_b
                                        .clone()
                                        .with(libp2p::multiaddr::Protocol::P2p(provider));
                                    swarm_c.dial(provider_addr).expect("C dial provider");
                                } else {
                                    connected_to_provider = true;
                                }
                            }
                        }
                        SwarmEvent::ConnectionEstablished { peer_id, .. }
                            if Some(peer_id) == discovered_provider && !connected_to_provider =>
                        {
                            connected_to_provider = true;
                        }
                        _ => {}
                    }
                }
                // Retry get_providers periodically (DHT may need time to propagate)
                _ = retry_interval.tick(), if queried_providers && discovered_provider.is_none() => {
                    let key = cli::service::service_key("echo");
                    let record_key = kad::RecordKey::new(&key);
                    swarm_c.behaviour_mut().kademlia.get_providers(record_key);
                }
            }

            // Once connected to the discovered provider, open tunnel and verify echo
            if connected_to_provider && let Some(provider) = discovered_provider {
                use futures::io::{AsyncReadExt as _, AsyncWriteExt as _};

                let mut stream = control_c
                    .open_stream(provider, cli::protocol::tunnel_protocol())
                    .await
                    .expect("open tunnel stream to provider");

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

                let mut len_buf = [0u8; 4];
                stream
                    .read_exact(&mut len_buf)
                    .await
                    .expect("read response length");
                let resp_len =
                    usize::try_from(u32::from_be_bytes(len_buf)).expect("u32 fits in usize");
                let mut resp_buf = vec![0u8; resp_len];
                stream
                    .read_exact(&mut resp_buf)
                    .await
                    .expect("read response body");
                let resp: cli::tunnel::TunnelResponse =
                    serde_json::from_slice(&resp_buf).expect("deserialize tunnel response");
                assert!(resp.accepted, "tunnel should be accepted");

                let test_data = b"hello service discovery!";
                stream.write_all(test_data).await.expect("write test data");
                stream.flush().await.expect("flush test data");

                let mut echo_buf = vec![0u8; test_data.len()];
                stream
                    .read_exact(&mut echo_buf)
                    .await
                    .expect("read echo response");

                if let Some(tx) = tx_result.take() {
                    let _ = tx.send((provider, echo_buf));
                }
                return;
            }
        }
    });

    // Wait for C to discover B, connect, and tunnel echo data
    let (discovered_provider, echo_data) =
        tokio::time::timeout(Duration::from_secs(30), rx_c_result)
            .await
            .expect("C service discovery + tunnel timed out")
            .expect("C result channel closed");

    assert_eq!(
        discovered_provider, peer_b,
        "discovered provider should be Peer B"
    );

    let test_data = b"hello service discovery!";
    assert_eq!(&echo_data, test_data, "echo data should match");

    // Clean up
    handle_a.abort();
    handle_b.abort();
    handle_c.abort();
    echo_handle.abort();
}

// ─── Test 3: Three-peer tunnel test ─────────────────────────────────────────

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
