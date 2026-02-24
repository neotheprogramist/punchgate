# UPnP/NAT-PMP/PCP Port Mapping Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add UPnP/NAT-PMP/PCP port mapping so NATed peers can accept inbound connections without relay.

**Architecture:** New `port_mapping` module races all three protocols in parallel (5s timeout), returns first success. Spawned after `swarm.listen_on()` when bootstrap peers are configured. On success, emits `NatStatusChanged(Public)` to skip relay reservation. On failure, existing relay flow proceeds unchanged.

**Tech Stack:** `igd-next` (UPnP IGD, tokio async), `crab_nat` (NAT-PMP + PCP, tokio), `default-net` (gateway detection)

---

### Task 1: Add dependencies

**Files:**
- Modify: `Cargo.toml` (workspace root, lines 9-39)
- Modify: `cli/Cargo.toml` (lines 11-25)

**Step 1: Add workspace dependencies**

Add to `[workspace.dependencies]` in root `Cargo.toml`:

```toml
igd-next = { version = "0.16", features = ["aio_tokio"] }
crab_nat = "0.7"
default-net = "0.23"
```

**Step 2: Add to cli dependencies**

Add to `[dependencies]` in `cli/Cargo.toml`:

```toml
igd-next.workspace = true
crab_nat.workspace = true
default-net.workspace = true
```

**Step 3: Verify compilation**

Run: `cargo build -p cli`
Expected: compiles with new dependencies

**Step 4: Commit**

```
feat(cli): add UPnP/NAT-PMP/PCP port mapping dependencies
```

---

### Task 2: Create port_mapping module with types and gateway detection

**Files:**
- Create: `cli/src/port_mapping.rs`
- Modify: `cli/src/lib.rs:4` — add `pub mod port_mapping;`

**Step 1: Write the module with types and gateway helper**

Create `cli/src/port_mapping.rs`:

```rust
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};

use anyhow::{Context, Result};
use tracing::{info, warn};

#[derive(Debug, Clone)]
pub struct PortMapping {
    pub external_addr: SocketAddr,
    pub protocol_used: &'static str,
}

fn detect_gateway() -> Result<Ipv4Addr> {
    let gw = default_net::get_default_gateway()
        .context("failed to detect default gateway")?;
    gw.ipv4
        .first()
        .copied()
        .context("default gateway has no IPv4 address")
}

fn detect_local_addr() -> Result<Ipv4Addr> {
    let ifaces = default_net::get_interfaces();
    ifaces
        .iter()
        .filter(|i| !i.is_loopback())
        .flat_map(|i| &i.ipv4)
        .map(|net| net.addr)
        .find(|ip| !ip.is_loopback() && !ip.is_link_local())
        .context("no non-loopback IPv4 address found")
}
```

**Step 2: Register module**

Add `pub mod port_mapping;` to `cli/src/lib.rs` after `pub mod nat_probe_protocol;`.

**Step 3: Verify compilation**

Run: `cargo build -p cli`
Expected: compiles

**Step 4: Commit**

```
feat(cli): add port_mapping module with types and gateway detection
```

---

### Task 3: Implement UPnP probe

**Files:**
- Modify: `cli/src/port_mapping.rs`

**Step 1: Add UPnP probe function**

Add to `port_mapping.rs`:

```rust
use std::time::Duration;

const PORT_MAPPING_TIMEOUT: Duration = Duration::from_secs(5);
const UPNP_DESCRIPTION: &str = "punchgate";

async fn try_upnp(internal_port: u16, local_ip: Ipv4Addr) -> Result<PortMapping> {
    let gateway = igd_next::aio::tokio::search_gateway(igd_next::SearchOptions {
        timeout: Some(PORT_MAPPING_TIMEOUT),
        ..Default::default()
    })
    .await
    .context("UPnP gateway search failed")?;

    let external_ip = gateway
        .get_external_ip()
        .await
        .context("UPnP: failed to get external IP")?;

    let local_addr = SocketAddrV4::new(local_ip, internal_port);

    gateway
        .add_port(
            igd_next::PortMappingProtocol::UDP,
            internal_port,
            local_addr,
            0, // indefinite lease
            UPNP_DESCRIPTION,
        )
        .await
        .context("UPnP: failed to add port mapping")?;

    let external_addr = SocketAddr::new(IpAddr::V4(external_ip), internal_port);
    info!(%external_addr, internal_port, "UPnP port mapping acquired");
    Ok(PortMapping {
        external_addr,
        protocol_used: "UPnP",
    })
}
```

**Step 2: Verify compilation**

Run: `cargo build -p cli`
Expected: compiles

**Step 3: Commit**

```
feat(cli): implement UPnP port mapping probe
```

---

### Task 4: Implement NAT-PMP/PCP probe

**Files:**
- Modify: `cli/src/port_mapping.rs`

**Step 1: Add NAT-PMP/PCP probe function**

Add to `port_mapping.rs`:

```rust
use std::num::NonZeroU16;

async fn try_nat_pmp_pcp(
    internal_port: u16,
    gateway_ip: Ipv4Addr,
    local_ip: Ipv4Addr,
) -> Result<PortMapping> {
    let port = NonZeroU16::new(internal_port)
        .context("internal port must be non-zero")?;

    let gateway_addr = SocketAddr::new(IpAddr::V4(gateway_ip), 5351);
    let client_addr = SocketAddr::new(IpAddr::V4(local_ip), 0);

    let mapping = crab_nat::PortMapping::new(
        gateway_addr,
        client_addr,
        crab_nat::InternetProtocol::Udp,
        port,
        crab_nat::PortMappingOptions::default(),
    )
    .await
    .map_err(|e| anyhow::anyhow!("NAT-PMP/PCP mapping failed: {e:?}"))?;

    let external_addr = mapping.external_address();
    let protocol_used = match mapping.mapping_type() {
        crab_nat::PortMappingType::Pcp => "PCP",
        crab_nat::PortMappingType::NatPmp => "NAT-PMP",
    };

    info!(%external_addr, %protocol_used, internal_port, "NAT-PMP/PCP port mapping acquired");
    Ok(PortMapping {
        external_addr,
        protocol_used,
    })
}
```

**Step 2: Verify compilation**

Run: `cargo build -p cli`
Expected: compiles

**Step 3: Commit**

```
feat(cli): implement NAT-PMP/PCP port mapping probe
```

---

### Task 5: Implement the parallel race entry point

**Files:**
- Modify: `cli/src/port_mapping.rs`

**Step 1: Add the public `acquire_port_mapping` function**

Add to `port_mapping.rs`:

```rust
pub async fn acquire_port_mapping(internal_port: u16) -> Option<PortMapping> {
    let gateway_ip = match detect_gateway() {
        Ok(ip) => {
            info!(%ip, "detected default gateway");
            ip
        }
        Err(e) => {
            warn!("gateway detection failed: {e:#}");
            return None;
        }
    };

    let local_ip = match detect_local_addr() {
        Ok(ip) => {
            info!(%ip, "detected local address");
            ip
        }
        Err(e) => {
            warn!("local address detection failed: {e:#}");
            return None;
        }
    };

    let upnp = try_upnp(internal_port, local_ip);
    let nat_pmp = try_nat_pmp_pcp(internal_port, gateway_ip, local_ip);

    tokio::pin!(upnp);
    tokio::pin!(nat_pmp);

    let result = tokio::time::timeout(PORT_MAPPING_TIMEOUT, async {
        tokio::select! {
            result = &mut upnp => match result {
                Ok(mapping) => Some(mapping),
                Err(e) => {
                    warn!("UPnP failed: {e:#}");
                    // Wait for NAT-PMP/PCP
                    match (&mut nat_pmp).await {
                        Ok(mapping) => Some(mapping),
                        Err(e2) => {
                            warn!("NAT-PMP/PCP failed: {e2:#}");
                            None
                        }
                    }
                }
            },
            result = &mut nat_pmp => match result {
                Ok(mapping) => Some(mapping),
                Err(e) => {
                    warn!("NAT-PMP/PCP failed: {e:#}");
                    // Wait for UPnP
                    match (&mut upnp).await {
                        Ok(mapping) => Some(mapping),
                        Err(e2) => {
                            warn!("UPnP failed: {e2:#}");
                            None
                        }
                    }
                }
            },
        }
    })
    .await;

    match result {
        Ok(Some(mapping)) => {
            info!(
                external_addr = %mapping.external_addr,
                protocol = mapping.protocol_used,
                "port mapping acquired"
            );
            Some(mapping)
        }
        Ok(None) => {
            info!("no port mapping available, falling back to relay");
            None
        }
        Err(_) => {
            warn!("port mapping timed out after {}s", PORT_MAPPING_TIMEOUT.as_secs());
            None
        }
    }
}
```

**Step 2: Verify compilation**

Run: `cargo build -p cli`
Expected: compiles

**Step 3: Commit**

```
feat(cli): implement parallel port mapping race (UPnP vs NAT-PMP/PCP)
```

---

### Task 6: Integrate port mapping into node startup

**Files:**
- Modify: `cli/src/node/mod.rs:148-184`

This is the most critical task. The port mapping must be spawned after `swarm.listen_on()` returns the actual OS-assigned port, and the result must be checked before the relay request path.

**Step 1: Extract QUIC listen port after listen_on**

After the `swarm.listen_on()` loop (line 150), add port extraction logic. The listen port comes from `SwarmEvent::NewListenAddr` events, but those arrive asynchronously. Instead, parse the listen addr: if it's `/ip4/0.0.0.0/udp/0/quic-v1`, poll the swarm once to get the actual port from `NewListenAddr`.

A simpler approach: spawn port mapping from within the event loop when we see the first `NewListenAddr` with a non-zero UDP port. Use a `Option<JoinHandle>` to track the spawned task.

**Step 2: Add port mapping handle and state to event loop setup**

In `cli/src/node/mod.rs`, after `let mut relayed_connections` (line 198), add:

```rust
let mut port_mapping_handle: Option<tokio::task::JoinHandle<Option<crate::port_mapping::PortMapping>>> = None;
let mut port_mapping_spawned = false;
let mut port_mapping_result: Option<crate::port_mapping::PortMapping> = None;
```

**Step 3: Spawn port mapping on first NewListenAddr**

Inside the event loop, after the `NewListenAddr` log (line 211), add:

```rust
if let SwarmEvent::NewListenAddr { address, .. } = &event
    && !port_mapping_spawned
    && !config.bootstrap_addrs.is_empty()
{
    if let Some(port) = extract_quic_udp_port(address) {
        port_mapping_spawned = true;
        let handle = tokio::spawn(crate::port_mapping::acquire_port_mapping(port));
        port_mapping_handle = Some(handle);
        tracing::info!(port, "spawned port mapping probe");
    }
}
```

Add helper at bottom of file:

```rust
fn extract_quic_udp_port(addr: &Multiaddr) -> Option<u16> {
    addr.iter().find_map(|p| match p {
        libp2p::multiaddr::Protocol::Udp(port) => Some(port),
        _ => None,
    })
}
```

**Step 4: Check port mapping result before relay request**

The relay request is triggered by `PeerIdentified` → `maybe_request_relay()` in the state machine. We need to intercept: if port mapping succeeded, emit `NatStatusChanged(Public)` instead, which prevents relay.

In the event loop, before the state machine transition (currently line 366 area: `let (new_state, commands) = app_state.transition(state_event);`), add a check:

```rust
// Check if port mapping completed
if let Some(handle) = &port_mapping_handle {
    if handle.is_finished() {
        if let Some(handle) = port_mapping_handle.take() {
            match handle.await {
                Ok(Some(mapping)) => {
                    port_mapping_result = Some(mapping.clone());
                    let mapped_addr: Multiaddr = format!(
                        "/ip4/{}/udp/{}/quic-v1",
                        mapping.external_addr.ip(),
                        mapping.external_addr.port()
                    )
                    .parse()
                    .expect("formatted multiaddr from SocketAddr is always valid");

                    swarm.add_external_address(mapped_addr.clone());
                    tracing::info!(
                        addr = %mapped_addr,
                        protocol = mapping.protocol_used,
                        "port mapping → added external address, switching to Public NAT"
                    );

                    let old_phase = app_state.phase();
                    let (new_state, commands) =
                        app_state.transition(Event::NatStatusChanged(NatStatus::Public));
                    log_phase_transition(old_phase, new_state.phase(), commands.len());
                    app_state = new_state;
                    execute_commands(&mut swarm, &commands, &config.exposed, &mut ctx);
                }
                Ok(None) => {
                    tracing::info!("port mapping unavailable, relay path will handle connectivity");
                }
                Err(e) => {
                    tracing::warn!(error = %e, "port mapping task panicked");
                }
            }
        }
    }
}
```

Place this right before the `translate_swarm_event` call in the event loop (before line 291 area).

**Step 5: Verify compilation and all tests pass**

Run: `cargo test -p cli --lib && cargo build -p cli --release`
Expected: 105+ tests pass, clean release build

**Step 6: Commit**

```
feat(cli): integrate port mapping into startup flow
```

---

### Task 7: Run clippy and fix issues

**Files:**
- Potentially any file modified above

**Step 1: Run clippy**

Run: `cargo +nightly clippy --workspace --all-features --all-targets`
Expected: no warnings

**Step 2: Fix any clippy issues**

**Step 3: Commit (if changes needed)**

```
fix(cli): resolve clippy warnings in port mapping
```

---

### Task 8: Manual deployment test

This is not code — it's a deployment verification on the 3 machines.

**Step 1: Build release on each machine**

```bash
cargo build --release
```

**Step 2: Start bootstrap (myszur)**

```bash
cargo run --release
```

Expected log: `no bootstrap peers, skipping NAT probe`

**Step 3: Start bycur (workhorse)**

```bash
cargo run --release -- --bootstrap /ip4/81.219.135.164/udp/4001/quic-v1/p2p/12D3KooWD8ZvJvEZ2aEXxonzcrbtgBw8ZK7NfuVLrHvX2dJQs6W3 --expose bycur-ssh=localhost:22
```

Observe logs for one of:
- `UPnP port mapping acquired` or `NAT-PMP/PCP port mapping acquired` → **success, direct connections will work**
- `no port mapping available, falling back to relay` → **UPnP/NAT-PMP not available on this router**

**Step 4: Start local (your machine)**

```bash
cargo run --release -- --bootstrap /ip4/81.219.135.164/udp/4001/quic-v1/p2p/12D3KooWD8ZvJvEZ2aEXxonzcrbtgBw8ZK7NfuVLrHvX2dJQs6W3 --tunnel 12D3KooWJcJEadnZQzkChrKNkczWjB9grYasXYvQ4LMGmreqEPrw:bycur-ssh@localhost:2222
```

Observe:
- If port mapping succeeded on either side: connection should be direct (no relay circuit in endpoint address)
- If port mapping failed on both: falls back to relay as before (no regression)
