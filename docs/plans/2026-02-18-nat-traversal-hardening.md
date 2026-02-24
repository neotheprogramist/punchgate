# NAT Traversal Hardening — Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Fix hole-punch failures on restricted-filtering cone NATs by adding CGNAT detection, NAT priming, retry on failure, and primed direct dial on retry.

**Architecture:** Four independent, composable layers. CGNAT guard validates port mapping results. NAT priming sends UDP packets via SO_REUSEPORT to create outbound NAT mappings before DCUtR. Retry gap fix ensures HolePunchFailed schedules retries. Primed direct dial bypasses DCUtR on retry, dialing the peer's observed external address after priming.

**Tech Stack:** tokio (UDP sockets, SO_REUSEPORT), libp2p (swarm, multiaddr), std::net

---

### Task 1: Add CGNAT/Private IP Guard to Port Mapping

**Files:**
- Modify: `cli/src/port_mapping.rs:38-65` (try_upnp) and `cli/src/port_mapping.rs:67-102` (try_nat_pmp_pcp)

**Step 1: Add `is_publicly_routable` function**

Add after the `PortMapping` struct (after line 16) in `cli/src/port_mapping.rs`:

```rust
fn is_publicly_routable(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            !v4.is_private()
                && !v4.is_loopback()
                && !v4.is_link_local()
                && !v4.is_unspecified()
                && !is_shared_address(v4)
        }
        IpAddr::V6(v6) => !v6.is_loopback() && !v6.is_unspecified(),
    }
}

fn is_shared_address(ip: Ipv4Addr) -> bool {
    let octets = ip.octets();
    // 100.64.0.0/10 — CGNAT (RFC 6598)
    octets[0] == 100 && (octets[1] & 0xC0) == 64
}
```

Note: `IpAddr` is already imported at line 1.

**Step 2: Add validation in `try_upnp`**

In `cli/src/port_mapping.rs`, after line 49 (`get_external_ip()` call), add before `gw.add_port(...)`:

```rust
    if !is_publicly_routable(ext_ip.into()) {
        anyhow::bail!("UPnP returned non-routable address {ext_ip} (likely CGNAT/double NAT)");
    }
```

Note: `igd_next::get_external_ip()` returns `Ipv4Addr`, so wrap with `.into()` for the `IpAddr` parameter.

**Step 3: Add validation in `try_nat_pmp_pcp`**

In `cli/src/port_mapping.rs`, after the `match mapping.mapping_type()` block (after line 96, before constructing `PortMapping`), add:

```rust
    if !is_publicly_routable(external_ip) {
        anyhow::bail!("NAT-PMP/PCP returned non-routable address {external_ip} (likely CGNAT/double NAT)");
    }
```

Here `external_ip` is already `IpAddr`.

**Step 4: Build and verify**

Run: `cargo build -p cli 2>&1 | tail -5`
Expected: Compiles without errors.

**Step 5: Run tests**

Run: `cargo test -p cli 2>&1 | tail -20`
Expected: All existing tests pass. (Port mapping has no unit tests — it requires real network hardware. The validation is pure logic tested implicitly by the integration tests.)

**Step 6: Commit**

```bash
git add cli/src/port_mapping.rs
git commit -m "fix(cli): reject CGNAT/private IPs from port mapping results"
```

---

### Task 2: Add New Commands to State Machine

**Files:**
- Modify: `cli/src/state/command.rs:8-49`

**Step 1: Add `PrimeNatMapping` and `PrimeAndDialDirect` variants**

In `cli/src/state/command.rs`, add after `AwaitHolePunch` (after line 45) and replace `RetryDirectDial`:

The new command enum should have these tunnel variants (lines 25-end):

```rust
    // ─── Tunnel (from TunnelState) ──────────────────────────────────────────
    PublishServices,
    DhtLookupPeer {
        peer: PeerId,
    },
    DhtGetProviders {
        service_name: ServiceName,
        key: KademliaKey,
    },
    DialPeer {
        peer: PeerId,
    },
    SpawnTunnel {
        peer: PeerId,
        service: ServiceName,
        bind: ServiceAddr,
        relayed: bool,
    },
    AwaitHolePunch {
        peer: PeerId,
    },
    PrimeNatMapping {
        peer: PeerId,
        peer_addrs: Vec<Multiaddr>,
    },
    PrimeAndDialDirect {
        peer: PeerId,
        peer_addrs: Vec<Multiaddr>,
    },
}
```

This removes `RetryDirectDial` and adds two new variants. `PrimeNatMapping` fires alongside `AwaitHolePunch`. `PrimeAndDialDirect` replaces `RetryDirectDial` on retry.

**Step 2: Build to find all compile errors from removing `RetryDirectDial`**

Run: `cargo build -p cli 2>&1 | grep "error\["`
Expected: Errors in `tunnel.rs` (line 295), `execute.rs` (line 125), `node/mod.rs` (line 570). These will be fixed in subsequent tasks.

---

### Task 3: Add Peer Address Tracking and NAT Priming Helper

**Files:**
- Create: `cli/src/nat_primer.rs`
- Modify: `cli/src/lib.rs` (add module)

**Step 1: Create the NAT primer module**

Create `cli/src/nat_primer.rs`:

```rust
use std::net::SocketAddr;

use libp2p::{Multiaddr, multiaddr::Protocol};
use tokio::net::UdpSocket;
use tracing::{debug, warn};

const PRIME_PACKET: &[u8] = b"punch";
const PRIME_COUNT: usize = 3;

fn extract_udp_socket_addr(addr: &Multiaddr) -> Option<SocketAddr> {
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

fn find_local_quic_addr(listeners: impl Iterator<Item = Multiaddr>) -> Option<SocketAddr> {
    listeners.filter_map(|addr| extract_udp_socket_addr(&addr)).find(|sa| {
        match sa.ip() {
            std::net::IpAddr::V4(v4) => !v4.is_loopback() && !v4.is_unspecified(),
            std::net::IpAddr::V6(v6) => !v6.is_loopback() && !v6.is_unspecified(),
        }
    })
}

pub async fn send_nat_priming(
    local_listen_addr: SocketAddr,
    peer_addrs: &[Multiaddr],
) {
    let targets: Vec<SocketAddr> = peer_addrs
        .iter()
        .filter_map(|a| extract_udp_socket_addr(a))
        .filter(|sa| {
            !sa.ip().is_loopback()
                && !sa.ip().is_unspecified()
                && !peer_addrs.iter().any(|a| {
                    a.iter()
                        .any(|p| matches!(p, Protocol::P2pCircuit))
                })
        })
        .collect();

    if targets.is_empty() {
        debug!("NAT priming: no direct peer addresses to prime");
        return;
    }

    let socket = match UdpSocket::bind(local_listen_addr).await {
        Ok(s) => s,
        Err(e) => {
            warn!(addr = %local_listen_addr, error = %e, "NAT priming: failed to bind SO_REUSEPORT socket");
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

pub fn resolve_priming_target(
    peer_addrs: &[Multiaddr],
    listeners: impl Iterator<Item = Multiaddr>,
) -> Option<SocketAddr> {
    find_local_quic_addr(listeners)
}
```

Note: `SO_REUSEPORT` is set automatically by `tokio::net::UdpSocket::bind` on most platforms when binding to an address already in use, but we need to set it explicitly. Actually, we need `socket2` for this. Let me adjust — use `socket2::Socket` to create the socket with `SO_REUSEPORT`, then convert to `tokio::net::UdpSocket`.

Revised socket creation section:

```rust
    let std_socket = {
        let domain = match local_listen_addr {
            SocketAddr::V4(_) => socket2::Domain::IPV4,
            SocketAddr::V6(_) => socket2::Domain::IPV6,
        };
        let socket = match socket2::Socket::new(domain, socket2::Type::DGRAM, Some(socket2::Protocol::UDP)) {
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
```

**Step 2: Add socket2 dependency**

Add to `Cargo.toml` workspace dependencies:
```toml
socket2 = { version = "0.5", features = ["all"] }
```

Add to `cli/Cargo.toml` dependencies:
```toml
socket2.workspace = true
```

**Step 3: Register the module**

Add `pub mod nat_primer;` to `cli/src/lib.rs` after `pub mod nat_probe_protocol;`.

**Step 4: Build**

Run: `cargo build -p cli 2>&1 | tail -5`
Expected: Compiles (there will still be errors from Task 2's `RetryDirectDial` removal, but the new module itself should compile).

---

### Task 4: Update TunnelState to Emit Priming Commands

**Files:**
- Modify: `cli/src/state/tunnel.rs:80-112` (route_connected_peer), `cli/src/state/tunnel.rs:178-197` (TunnelPeerConnected relayed:true), `cli/src/state/tunnel.rs:293-297` (HolePunchRetryTick)

**Step 1: Add `peer_external_addrs` field to TunnelState**

In `cli/src/state/tunnel.rs`, add a new field to the `TunnelState` struct:

```rust
    peer_external_addrs: HashMap<PeerId, Vec<Multiaddr>>,
```

Initialize as `HashMap::new()` in the constructor.

**Step 2: Store peer addresses from PeerIdentified**

In the `TunnelState::transition` match arm for `Event::PeerIdentified`, store the peer's listen addresses:

```rust
Event::PeerIdentified { peer, listen_addrs, .. } => {
    if !listen_addrs.is_empty() {
        self.peer_external_addrs.insert(peer, listen_addrs.clone());
    }
}
```

Note: `TunnelState` currently doesn't handle `PeerIdentified` — the event falls through to the `_ => {}` catch-all. Add this before the catch-all.

**Step 3: Emit PrimeNatMapping alongside AwaitHolePunch in route_connected_peer**

In `cli/src/state/tunnel.rs`, `route_connected_peer` (line 95-100), change the relayed branch:

```rust
        } else if self.relayed_peers.contains(&peer) {
            self.awaiting_holepunch
                .entry(peer)
                .or_default()
                .extend(specs);
            let peer_addrs = self.peer_external_addrs.get(&peer).cloned().unwrap_or_default();
            vec![
                Command::PrimeNatMapping { peer, peer_addrs },
                Command::AwaitHolePunch { peer },
            ]
```

**Step 4: Emit PrimeNatMapping alongside AwaitHolePunch in TunnelPeerConnected**

In `cli/src/state/tunnel.rs` line 184-186, change the relayed:true + holepunch-viable branch:

```rust
                            if self.nat_mapping.is_holepunch_viable() {
                                self.awaiting_holepunch.insert(peer, specs);
                                let peer_addrs = self.peer_external_addrs.get(&peer).cloned().unwrap_or_default();
                                commands.push(Command::PrimeNatMapping { peer, peer_addrs: peer_addrs.clone() });
                                commands.push(Command::AwaitHolePunch { peer });
```

**Step 5: Replace RetryDirectDial with PrimeAndDialDirect in HolePunchRetryTick**

In `cli/src/state/tunnel.rs` lines 293-297, change:

```rust
            Event::HolePunchRetryTick { peer } => {
                if self.relayed_peers.contains(&peer) {
                    let peer_addrs = self.peer_external_addrs.get(&peer).cloned().unwrap_or_default();
                    commands.push(Command::PrimeAndDialDirect { peer, peer_addrs });
                }
            }
```

**Step 6: Clean up peer addresses on ConnectionLost**

In the `ConnectionLost { remaining_connections: 0 }` handler (line 276-291), add:

```rust
                self.peer_external_addrs.remove(&peer);
```

**Step 7: Build**

Run: `cargo build -p cli 2>&1 | grep "error\["`
Expected: Only errors in `execute.rs` and `node/mod.rs` (from `RetryDirectDial` removal). Tunnel state should compile.

---

### Task 5: Update execute_commands for New Commands

**Files:**
- Modify: `cli/src/node/execute.rs:117-135`

**Step 1: Replace AwaitHolePunch handler to also support priming**

In `cli/src/node/execute.rs`, replace the `AwaitHolePunch` handler (lines 117-124) with:

```rust
            Command::AwaitHolePunch { peer } => {
                let external_addrs: Vec<_> = swarm.external_addresses().collect();
                tracing::info!(
                    %peer,
                    addrs = ?external_addrs,
                    "awaiting hole-punch — external address snapshot"
                );
            }
            Command::PrimeNatMapping { peer, peer_addrs } => {
                let local_addr = swarm
                    .listeners()
                    .filter_map(|a| crate::nat_primer::extract_udp_socket_addr(a))
                    .find(|sa| match sa.ip() {
                        std::net::IpAddr::V4(v4) => !v4.is_loopback() && !v4.is_unspecified(),
                        std::net::IpAddr::V6(v6) => !v6.is_loopback() && !v6.is_unspecified(),
                    });

                if let Some(local_addr) = local_addr {
                    let addrs = peer_addrs.clone();
                    tokio::spawn(async move {
                        crate::nat_primer::send_nat_priming(local_addr, &addrs).await;
                    });
                    tracing::info!(%peer, %local_addr, "NAT priming packets sent");
                } else {
                    tracing::debug!(%peer, "NAT priming skipped — no non-loopback listen address");
                }
            }
```

Note: make `extract_udp_socket_addr` pub in `nat_primer.rs`.

**Step 2: Replace RetryDirectDial with PrimeAndDialDirect handler**

Replace lines 125-135 with:

```rust
            Command::PrimeAndDialDirect { peer, peer_addrs } => {
                if swarm.is_connected(peer) {
                    tracing::debug!(%peer, "skipping primed direct dial — already connected");
                } else {
                    let local_addr = swarm
                        .listeners()
                        .filter_map(|a| crate::nat_primer::extract_udp_socket_addr(a))
                        .find(|sa| match sa.ip() {
                            std::net::IpAddr::V4(v4) => !v4.is_loopback() && !v4.is_unspecified(),
                            std::net::IpAddr::V6(v6) => !v6.is_loopback() && !v6.is_unspecified(),
                        });

                    if let Some(local_addr) = local_addr {
                        let addrs = peer_addrs.clone();
                        tokio::spawn(async move {
                            crate::nat_primer::send_nat_priming(local_addr, &addrs).await;
                        });
                        tracing::info!(%peer, %local_addr, "NAT priming packets sent before direct dial");
                    }

                    // Dial the peer's observed external addresses directly (bypass relay/DCUtR)
                    for addr in peer_addrs {
                        if !addr.iter().any(|p| matches!(p, libp2p::multiaddr::Protocol::P2pCircuit)) {
                            tracing::info!(%peer, %addr, "primed direct dial attempt");
                            match swarm.dial(addr.clone()) {
                                Ok(()) => break,
                                Err(e) => tracing::debug!(%peer, error = %e, "primed direct dial failed"),
                            }
                        }
                    }
                }
            }
```

**Step 3: Build**

Run: `cargo build -p cli 2>&1 | grep "error\["`
Expected: Only error in `node/mod.rs` line 570 where it still matches `RetryDirectDial`.

---

### Task 6: Fix Retry Gap and Update Event Loop

**Files:**
- Modify: `cli/src/node/mod.rs:419-427` (HolePunchFailed handler) and `cli/src/node/mod.rs:570` (retry re-scheduling)

**Step 1: Schedule retry on HolePunchFailed**

In `cli/src/node/mod.rs`, after line 426 (`holepunch_deadlines.remove(remote_peer);`), add:

```rust
                            holepunch_retries.entry(*remote_peer)
                                .or_insert(tokio::time::Instant::now() + HOLEPUNCH_RETRY_INTERVAL);
```

**Step 2: Update retry re-scheduling to match new command**

In `cli/src/node/mod.rs`, line 570, change the command match from `RetryDirectDial` to `PrimeAndDialDirect`:

```rust
                    if commands.iter().any(|c| matches!(c, Command::PrimeAndDialDirect { .. })) {
                        holepunch_retries.insert(peer, tokio::time::Instant::now() + HOLEPUNCH_RETRY_INTERVAL);
                    }
```

**Step 3: Build**

Run: `cargo build -p cli 2>&1 | tail -5`
Expected: Compiles with zero errors. All `RetryDirectDial` references are now replaced.

**Step 4: Run clippy**

Run: `cargo clippy -p cli 2>&1 | tail -20`
Expected: No warnings (fix any that appear).

**Step 5: Run all tests**

Run: `cargo test -p cli 2>&1 | tail -20`
Expected: All tests pass.

**Step 6: Commit all changes**

```bash
git add cli/src/port_mapping.rs cli/src/nat_primer.rs cli/src/lib.rs \
        cli/src/state/command.rs cli/src/state/tunnel.rs \
        cli/src/node/mod.rs cli/src/node/execute.rs \
        Cargo.toml cli/Cargo.toml Cargo.lock
git commit -m "feat(cli): add NAT priming and fix hole-punch retry for restricted filtering NATs

Send UDP priming packets via SO_REUSEPORT to peer's observed external
address before DCUtR, creating outbound NAT mappings. On retry, bypass
DCUtR and dial the peer directly after priming. Also fix bug where
HolePunchFailed never scheduled retries."
```

---

### Task 7: Run E2E Tests

**Files:** None (verification only)

**Step 1: Run unit/integration tests**

Run: `cargo test -p cli`
Expected: All tests pass.

**Step 2: Run LAN e2e test**

Run: `scripts/e2e_tunnel_test.sh`
Expected: PASS — echo through tunnel matches.

**Step 3: Run NAT compose e2e test**

Run: `scripts/e2e_compose_test.sh`
Expected: All 4 checks PASS — echo tunnel, NAT translation, DCUtR hole punch, direct tunnel.
