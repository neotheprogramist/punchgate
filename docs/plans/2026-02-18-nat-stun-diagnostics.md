# NAT STUN Diagnostics Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Detect NAT mapping type via STUN on startup, filter relay-polluted observations, and skip doomed hole-punches for symmetric NAT.

**Architecture:** STUN probe runs before swarm start on a temporary UDP socket, querying 2 public STUN servers. Result feeds into state machine as `NatMappingDetected` event. Relay-observed addresses are filtered by tracking relayed connection endpoints. Symmetric NAT detection bypasses the 15s hole-punch timeout.

**Tech Stack:** `stun` crate (webrtc-rs) for STUN message construction/parsing, `tokio::net::UdpSocket` for transport, existing coalgebraic state machine.

**Design doc:** `docs/plans/2026-02-18-nat-stun-diagnostics-design.md`

---

### Task 1: Add `stun` dependency

**Files:**
- Modify: `Cargo.toml` (workspace root)
- Modify: `cli/Cargo.toml`

**Step 1: Add stun to workspace dependencies**

In `Cargo.toml` workspace root, add to `[workspace.dependencies]`:

```toml
stun = "0.6"
```

Note: version may need adjustment — check `cargo add stun --dry-run` to find the latest compatible version. The `stun` crate from webrtc-rs provides `Message`, `BINDING_REQUEST`, and `XorMappedAddress`.

**Step 2: Add stun to cli dependencies**

In `cli/Cargo.toml`, add to `[dependencies]`:

```toml
stun.workspace = true
```

**Step 3: Verify it compiles**

Run: `cargo check -p cli`
Expected: compiles without errors

---

### Task 2: Create `NatMapping` type and probe module

**Files:**
- Create: `cli/src/nat_probe.rs`
- Modify: `cli/src/lib.rs`

**Step 1: Write the type definitions**

Create `cli/src/nat_probe.rs` with the `NatMapping` enum and probe function signature:

```rust
use std::{net::SocketAddr, time::Duration};

use anyhow::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, strum::Display)]
pub enum NatMapping {
    /// Same external port for all destinations — hole-punch viable
    #[strum(serialize = "endpoint-independent (cone NAT)")]
    EndpointIndependent,
    /// Different external port per destination — hole-punch will fail
    #[strum(serialize = "address-dependent (symmetric NAT)")]
    AddressDependent,
    /// Detection failed or not attempted
    #[strum(serialize = "unknown")]
    Unknown,
}

impl NatMapping {
    pub fn is_holepunch_viable(self) -> bool {
        match self {
            Self::EndpointIndependent => true,
            Self::AddressDependent => false,
            Self::Unknown => true, // optimistic — try hole-punch
        }
    }
}

pub const DEFAULT_STUN_SERVERS: &[&str] = &[
    "stun.l.google.com:19302",
    "stun.cloudflare.com:3478",
];

const STUN_TIMEOUT: Duration = Duration::from_secs(3);
```

**Step 2: Register the module**

In `cli/src/lib.rs`, add:

```rust
pub mod nat_probe;
```

**Step 3: Write the failing tests for NatMapping classification**

In `cli/src/nat_probe.rs`, add test module:

```rust
#[cfg(test)]
mod tests {
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
    fn unknown_is_optimistically_viable() {
        assert!(NatMapping::Unknown.is_holepunch_viable());
    }
}
```

**Step 4: Run tests**

Run: `cargo test -p cli --lib nat_probe`
Expected: PASS (type + tests are self-contained)

---

### Task 3: Implement STUN probe function

**Files:**
- Modify: `cli/src/nat_probe.rs`

**Step 1: Implement `detect_nat_mapping`**

Add the async probe function to `cli/src/nat_probe.rs`:

```rust
use tokio::net::UdpSocket;

/// Probe NAT mapping behavior by querying 2+ STUN servers from the same socket.
///
/// Compares external port mappings: same port = cone NAT, different ports = symmetric.
pub async fn detect_nat_mapping(stun_servers: &[&str]) -> NatMapping {
    match probe_stun_servers(stun_servers).await {
        Ok(addrs) if addrs.len() >= 2 => classify_mapping(&addrs),
        Ok(_) => {
            tracing::warn!("insufficient STUN responses for NAT classification");
            NatMapping::Unknown
        }
        Err(e) => {
            tracing::warn!(error = %e, "STUN probe failed, NAT type unknown");
            NatMapping::Unknown
        }
    }
}

fn classify_mapping(addrs: &[SocketAddr]) -> NatMapping {
    let first_port = addrs[0].port();
    let all_same = addrs.iter().all(|a| a.port() == first_port);
    match all_same {
        true => NatMapping::EndpointIndependent,
        false => NatMapping::AddressDependent,
    }
}

async fn probe_stun_servers(servers: &[&str]) -> Result<Vec<SocketAddr>> {
    let socket = UdpSocket::bind("0.0.0.0:0").await?;
    let mut results = Vec::new();

    for server in servers {
        match stun_binding_request(&socket, server).await {
            Ok(addr) => {
                tracing::info!(
                    stun_server = %server,
                    external_addr = %addr,
                    "STUN binding response"
                );
                results.push(addr);
            }
            Err(e) => {
                tracing::warn!(stun_server = %server, error = %e, "STUN request failed");
            }
        }
    }

    Ok(results)
}

async fn stun_binding_request(socket: &UdpSocket, server: &str) -> Result<SocketAddr> {
    use stun::message::{Message, BINDING_REQUEST, TransactionId};
    use stun::xoraddr::XorMappedAddress;

    let mut msg = Message::new();
    msg.build(&[
        Box::new(TransactionId::new()),
        Box::new(BINDING_REQUEST),
    ])?;

    socket.send_to(&msg.raw, server).await?;

    let mut buf = vec![0u8; 1500];
    let (n, _) = tokio::time::timeout(STUN_TIMEOUT, socket.recv_from(&mut buf)).await??;

    let mut resp = Message::new();
    resp.raw = buf[..n].to_vec();
    resp.decode()?;

    let mut xor_addr = XorMappedAddress::default();
    xor_addr.get_from(&resp)?;

    Ok(SocketAddr::new(xor_addr.ip, xor_addr.port))
}
```

**Important:** The exact `stun` crate API may differ from the above. Adjust `Message::build`, `TransactionId`, `XorMappedAddress` usage to match the actual crate version. Check `cargo doc --open -p stun` for the precise API.

**Step 2: Write the classification unit test**

Add to the test module in `nat_probe.rs`:

```rust
use proptest::prelude::*;

proptest! {
    #[test]
    fn classify_same_ports_is_endpoint_independent(
        ip_a in (1u8..=254, 0u8..=255, 0u8..=255, 1u8..=254),
        ip_b in (1u8..=254, 0u8..=255, 0u8..=255, 1u8..=254),
        port in 1024u16..65535u16,
    ) {
        let addrs = vec![
            SocketAddr::new(
                std::net::IpAddr::V4(std::net::Ipv4Addr::new(ip_a.0, ip_a.1, ip_a.2, ip_a.3)),
                port,
            ),
            SocketAddr::new(
                std::net::IpAddr::V4(std::net::Ipv4Addr::new(ip_b.0, ip_b.1, ip_b.2, ip_b.3)),
                port,
            ),
        ];
        prop_assert_eq!(classify_mapping(&addrs), NatMapping::EndpointIndependent);
    }

    #[test]
    fn classify_different_ports_is_address_dependent(
        ip_a in (1u8..=254, 0u8..=255, 0u8..=255, 1u8..=254),
        ip_b in (1u8..=254, 0u8..=255, 0u8..=255, 1u8..=254),
        port_a in 1024u16..32767u16,
        port_b in 32768u16..65535u16,
    ) {
        let addrs = vec![
            SocketAddr::new(
                std::net::IpAddr::V4(std::net::Ipv4Addr::new(ip_a.0, ip_a.1, ip_a.2, ip_a.3)),
                port_a,
            ),
            SocketAddr::new(
                std::net::IpAddr::V4(std::net::Ipv4Addr::new(ip_b.0, ip_b.1, ip_b.2, ip_b.3)),
                port_b,
            ),
        ];
        prop_assert_eq!(classify_mapping(&addrs), NatMapping::AddressDependent);
    }
}
```

**Step 3: Run tests**

Run: `cargo test -p cli --lib nat_probe`
Expected: PASS

---

### Task 4: Add `NatMappingDetected` event to state machine

**Files:**
- Modify: `cli/src/state/event.rs`
- Modify: `cli/src/state/peer.rs`
- Modify: `cli/src/state/mod.rs`

**Step 1: Write failing tests for NatMappingDetected transition in PeerState**

In `cli/src/state/peer.rs` test module, add:

```rust
#[test]
fn nat_mapping_detected_stores_mapping(
    mapping in prop_oneof![
        Just(NatMapping::EndpointIndependent),
        Just(NatMapping::AddressDependent),
        Just(NatMapping::Unknown),
    ],
) {
    let state = PeerState::new();
    let (state, _) = state.transition(Event::NatMappingDetected(mapping));
    prop_assert_eq!(state.nat_mapping, mapping);
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p cli --lib state::peer::tests::nat_mapping_detected`
Expected: FAIL — `NatMappingDetected` variant doesn't exist, `nat_mapping` field doesn't exist

**Step 3: Add the event variant**

In `cli/src/state/event.rs`, add to the `Event` enum in the "Network lifecycle" section:

```rust
NatMappingDetected(crate::nat_probe::NatMapping),
```

**Step 4: Add `nat_mapping` field to `PeerState`**

In `cli/src/state/peer.rs`:

Add field to `PeerState` struct:
```rust
pub nat_mapping: crate::nat_probe::NatMapping,
```

Initialize in `PeerState::new()`:
```rust
nat_mapping: crate::nat_probe::NatMapping::Unknown,
```

Add transition case in `MealyMachine for PeerState`:
```rust
Event::NatMappingDetected(mapping) => {
    self.nat_mapping = mapping;
}
```

Add `NatMappingDetected` to the no-op match arm in `TunnelState::transition()` (`cli/src/state/tunnel.rs`).

**Step 5: Re-export `NatMapping`**

In `cli/src/state/mod.rs`, add:
```rust
pub use crate::nat_probe::NatMapping;
```

**Step 6: Run tests**

Run: `cargo test -p cli --lib`
Expected: PASS — all existing + new tests pass

---

### Task 5: Filter relay-observed addresses from NAT tracking

**Files:**
- Modify: `cli/src/node/mod.rs`

**Step 1: Add `relayed_connections` tracking**

In `node::run()`, after the `observed_nat_ports` declaration (line 154), add:

```rust
let mut relayed_connections: HashSet<PeerId> = HashSet::new();
```

**Step 2: Populate on ConnectionEstablished**

In the existing `ConnectionEstablished` handler block (around line 169-174), add tracking:

```rust
if let SwarmEvent::ConnectionEstablished { peer_id, endpoint, .. } = &event {
    if is_relayed_endpoint(endpoint) {
        relayed_connections.insert(*peer_id);
    }
}
```

Where `is_relayed_endpoint` checks for `/p2p-circuit` in the endpoint address (reuse the existing `is_relayed` function from `translate.rs`, or add a helper).

**Step 3: Clean up on ConnectionClosed**

In the existing `ConnectionClosed` handler (around line 172-174), add:

```rust
if let SwarmEvent::ConnectionClosed { peer_id, num_established: 0, .. } = &event {
    relayed_connections.remove(peer_id);
}
```

**Step 4: Filter observations**

Modify the NAT observation block (lines 239-259) to skip relayed observers:

```rust
if let Event::PeerIdentified { peer, observed_addr, .. } = &state_event
    && !relayed_connections.contains(&peer)  // <-- NEW: skip relay-observed
    && let Some((ip, port)) = external_addr::extract_public_ip_port(observed_addr)
{
    // ... existing observation logic unchanged
}
```

**Step 5: Verify compilation and existing tests**

Run: `cargo test -p cli --lib`
Expected: PASS — existing tests unaffected, this is a runtime-only change

---

### Task 6: Skip hole-punch for symmetric NAT in TunnelState

**Files:**
- Modify: `cli/src/state/tunnel.rs`
- Modify: `cli/src/state/app.rs`

**Step 1: Write failing tests**

In `cli/src/state/tunnel.rs` test module, add:

```rust
#[test]
fn relayed_peer_skips_holepunch_when_symmetric_nat(
    peer in arb_peer_id(),
    service in arb_service_name(),
    bind in arb_service_addr(),
) {
    let mut state = TunnelState::new();
    state.set_nat_mapping(NatMapping::AddressDependent);
    state.relayed_peers.insert(peer);
    state.add_peer_tunnel(peer, service.clone(), bind.clone());

    let (state, commands) = state.transition(Event::DhtPeerLookupComplete {
        peer, connected: true,
    });

    // Should spawn on relay immediately, not await hole-punch
    let has_spawn = commands.contains(&Command::SpawnTunnel {
        peer, service, bind, relayed: true,
    });
    prop_assert!(has_spawn);
    prop_assert!(!state.awaiting_holepunch.contains_key(&peer));
}

#[test]
fn relayed_peer_awaits_holepunch_when_cone_nat(
    peer in arb_peer_id(),
    service in arb_service_name(),
    bind in arb_service_addr(),
) {
    let mut state = TunnelState::new();
    state.set_nat_mapping(NatMapping::EndpointIndependent);
    state.relayed_peers.insert(peer);
    state.add_peer_tunnel(peer, service.clone(), bind.clone());

    let (state, commands) = state.transition(Event::DhtPeerLookupComplete {
        peer, connected: true,
    });

    // Should await hole-punch as before
    let has_await = commands.contains(&Command::AwaitHolePunch { peer });
    prop_assert!(has_await);
    prop_assert!(state.awaiting_holepunch.contains_key(&peer));
}

#[test]
fn tunnel_connected_relayed_skips_holepunch_when_symmetric(
    peer in arb_peer_id(),
    service in arb_service_name(),
    bind in arb_service_addr(),
) {
    let mut state = TunnelState::new();
    state.set_nat_mapping(NatMapping::AddressDependent);
    state.add_peer_tunnel(peer, service.clone(), bind.clone());

    let (_, commands) = state.transition(Event::TunnelPeerConnected {
        peer, relayed: true,
    });

    // Should spawn on relay immediately
    let has_spawn = commands.contains(&Command::SpawnTunnel {
        peer, service, bind, relayed: true,
    });
    prop_assert!(has_spawn);
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test -p cli --lib state::tunnel::tests`
Expected: FAIL — `set_nat_mapping` doesn't exist

**Step 3: Add `nat_mapping` to TunnelState**

In `cli/src/state/tunnel.rs`, add field and setter:

```rust
use crate::nat_probe::NatMapping;

pub struct TunnelState {
    // ... existing fields ...
    nat_mapping: NatMapping,
}

impl TunnelState {
    pub fn new() -> Self {
        Self {
            // ... existing ...
            nat_mapping: NatMapping::Unknown,
        }
    }

    pub fn set_nat_mapping(&mut self, mapping: NatMapping) {
        self.nat_mapping = mapping;
    }
    // ... rest unchanged
}
```

**Step 4: Modify `route_connected_peer` to check NAT mapping**

In `route_connected_peer`, change the condition:

```rust
fn route_connected_peer(
    &mut self,
    peer: PeerId,
    specs: Vec<(ServiceName, ServiceAddr)>,
) -> Vec<Command> {
    if self.holepunch_failed.contains(&peer)
        || !self.nat_mapping.is_holepunch_viable()  // <-- NEW
    {
        specs
            .into_iter()
            .map(|(service, bind)| Command::SpawnTunnel {
                peer, service, bind, relayed: true,
            })
            .collect()
    } else if self.relayed_peers.contains(&peer) {
        // ... existing await holepunch logic
    } else {
        // ... existing direct spawn logic
    }
}
```

**Step 5: Modify `TunnelPeerConnected { relayed: true }` handler**

When a relayed connection arrives and NAT is symmetric, spawn on relay immediately instead of awaiting hole-punch:

```rust
Event::TunnelPeerConnected { peer, relayed } => {
    self.dialing.remove(&peer);
    match relayed {
        true => {
            self.relayed_peers.insert(peer);
            if let Some(specs) = self.pending_by_peer.remove(&peer) {
                if self.nat_mapping.is_holepunch_viable() {
                    self.awaiting_holepunch.insert(peer, specs);
                    commands.push(Command::AwaitHolePunch { peer });
                } else {
                    commands.extend(specs.into_iter().map(|(service, bind)| {
                        Command::SpawnTunnel {
                            peer, service, bind, relayed: true,
                        }
                    }));
                }
            }
        }
        false => {
            // ... existing direct connection logic unchanged
        }
    }
}
```

**Step 6: Forward NatMappingDetected to TunnelState**

In `cli/src/state/tunnel.rs`, add handler in the `transition` match:

```rust
Event::NatMappingDetected(mapping) => {
    self.nat_mapping = mapping;
}
```

Remove `NatMappingDetected` from the no-op arm added in Task 4.

**Step 7: Run tests**

Run: `cargo test -p cli --lib`
Expected: PASS — all existing + new tests pass

---

### Task 7: Integrate STUN probe into node startup

**Files:**
- Modify: `cli/src/node/mod.rs`

**Step 1: Add STUN probe before swarm construction**

In `node::run()`, after the `bootstrap_peer_ids` extraction (line ~70) and before swarm construction (line ~72), add:

```rust
let nat_mapping = match config.bootstrap_addrs.is_empty() {
    true => {
        tracing::info!("no bootstrap peers, skipping NAT probe");
        crate::nat_probe::NatMapping::Unknown
    }
    false => {
        tracing::info!("probing NAT type via STUN...");
        let mapping = crate::nat_probe::detect_nat_mapping(
            crate::nat_probe::DEFAULT_STUN_SERVERS,
        ).await;
        match mapping {
            crate::nat_probe::NatMapping::EndpointIndependent => {
                tracing::info!(nat_type = %mapping, "hole-punching viable");
            }
            crate::nat_probe::NatMapping::AddressDependent => {
                tracing::warn!(
                    nat_type = %mapping,
                    "hole-punching not viable, all tunnels will use relay transport"
                );
            }
            crate::nat_probe::NatMapping::Unknown => {
                tracing::warn!(nat_type = %mapping, "could not determine NAT type");
            }
        }
        mapping
    }
};
```

**Step 2: Feed NAT mapping into state machine**

After the `NatStatusChanged(Private)` transition for bootstrap peers (line ~141), add:

```rust
{
    let old_phase = app_state.phase();
    let (new_state, commands) = app_state.transition(Event::NatMappingDetected(nat_mapping));
    log_phase_transition(old_phase, new_state.phase(), commands.len());
    app_state = new_state;
    execute_commands(&mut swarm, &commands, &config.exposed, &mut ctx);
}
```

**Step 3: Verify compilation**

Run: `cargo check -p cli`
Expected: compiles without errors

**Step 4: Manual test**

Run the node with bootstrap to verify STUN probe works:

```bash
RUST_LOG=cli=info cargo run -p cli -- \
  --bootstrap /ip4/81.219.135.164/udp/4001/quic-v1/p2p/12D3KooWD8ZvJvEZ2aEXxonzcrbtgBw8ZK7NfuVLrHvX2dJQs6W3
```

Expected log output (one of):
```
INFO probing NAT type via STUN...
INFO STUN binding response stun_server=stun.l.google.com:19302 external_addr=185.235.206.23:XXXXX
INFO STUN binding response stun_server=stun.cloudflare.com:3478 external_addr=185.235.206.23:YYYYY
INFO nat_type=endpoint-independent (cone NAT) hole-punching viable
```
or:
```
WARN nat_type=address-dependent (symmetric NAT) hole-punching not viable, all tunnels will use relay transport
```

---

### Task 8: Update `nat_type_summary` to use STUN result

**Files:**
- Modify: `cli/src/node/mod.rs`

**Step 1: Pass NatMapping to summary function**

Replace the `nat_type_summary` function (line ~469) to use the authoritative STUN result when available:

```rust
fn nat_type_summary(
    stun_result: NatMapping,
    observations: &HashMap<IpAddr, HashSet<u16>>,
) -> String {
    match stun_result {
        NatMapping::EndpointIndependent => "cone NAT (STUN-verified)".to_string(),
        NatMapping::AddressDependent => "symmetric NAT (STUN-verified)".to_string(),
        NatMapping::Unknown => {
            let max_ports_per_ip = observations.values().map(|p| p.len()).max().unwrap_or(0);
            match max_ports_per_ip {
                0 => "no observations".to_string(),
                1 => "undetermined (single observer per IP)".to_string(),
                _ => "symmetric NAT likely (multiple ports per IP)".to_string(),
            }
        }
    }
}
```

**Step 2: Update all call sites**

Pass `nat_mapping` to `nat_type_summary` wherever it's called (hole-punch timeout log, hole-punch failed log).

**Step 3: Run all tests**

Run: `cargo test -p cli --lib`
Expected: PASS

---

## Summary of all files changed

| File | Action | Purpose |
|------|--------|---------|
| `Cargo.toml` | Modify | Add `stun` workspace dependency |
| `cli/Cargo.toml` | Modify | Add `stun` to cli dependencies |
| `cli/src/lib.rs` | Modify | Register `nat_probe` module |
| `cli/src/nat_probe.rs` | Create | STUN probe + NatMapping type |
| `cli/src/state/event.rs` | Modify | Add `NatMappingDetected` variant |
| `cli/src/state/peer.rs` | Modify | Add `nat_mapping` field + transition |
| `cli/src/state/tunnel.rs` | Modify | Skip hole-punch for symmetric NAT |
| `cli/src/state/mod.rs` | Modify | Re-export `NatMapping` |
| `cli/src/node/mod.rs` | Modify | STUN probe at startup, relay filtering, summary update |
