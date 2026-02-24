# NAT Hole-Punch Diagnostics Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Diagnose why DCUtR hole-punching fails between two cone-NAT peers by adding a STUN filtering probe, DCUtR address audit logging, and fixing the timeout race.

**Architecture:** Three layered diagnostics — (1) bump timeout to stop racing DCUtR, (2) add logging to trace what addresses DCUtR uses and what errors occur, (3) add a bootstrap-mediated UDP probe to classify NAT filtering type. Each layer gives more insight; deploy incrementally.

**Tech Stack:** Rust, libp2p 0.56, tokio, stun crate, serde (for probe messages over libp2p-stream)

---

### Task 1: Fix Timeout Race — Bump HOLEPUNCH_TIMEOUT

The 15s timeout fires 0.3s before DCUtR completes its 3-attempt cycle, causing the initiator to miss the DCUtR failure reason.

**Files:**
- Modify: `cli/src/protocol.rs:13`

**Step 1: Change the constant**

In `cli/src/protocol.rs`, change line 13:

```rust
// Before:
pub const HOLEPUNCH_TIMEOUT: Duration = Duration::from_secs(15);

// After:
pub const HOLEPUNCH_TIMEOUT: Duration = Duration::from_secs(30);
```

**Step 2: Run existing tests to verify no regressions**

Run: `cargo test -p cli --lib`
Expected: All pass (timeout constant isn't tested directly, but state machine tests reference `HOLEPUNCH_TIMEOUT` indirectly)

**Step 3: Commit**

```
feat(protocol): bump hole-punch timeout from 15s to 30s

DCUtR's 3-attempt QUIC handshake cycle takes ~15-18s. The previous
15s deadline raced with DCUtR, causing the initiator to miss the
failure reason.
```

---

### Task 2: Log External Addresses at Hole-Punch Start

When `AwaitHolePunch` fires, log the peer's known external addresses so we can verify DCUtR has correct candidates.

**Files:**
- Modify: `cli/src/node/execute.rs:117-119`

**Step 1: Add address snapshot logging**

In `cli/src/node/execute.rs`, the `AwaitHolePunch` handler is currently a no-op comment. The executor doesn't have access to the swarm's external addresses because `execute_commands` takes `&mut Swarm` — but we can log from there.

Change the function signature and the `AwaitHolePunch` arm:

In `execute_commands`, change the `AwaitHolePunch` arm at line 117-119:

```rust
// Before:
Command::AwaitHolePunch { .. } => {
    // Handled by the event loop's holepunch deadline tracking, not the executor
}

// After:
Command::AwaitHolePunch { peer } => {
    let external_addrs: Vec<_> = swarm.external_addresses().collect();
    tracing::info!(
        %peer,
        addrs = ?external_addrs,
        "awaiting hole-punch — external address snapshot"
    );
}
```

**Step 2: Run tests**

Run: `cargo test -p cli --lib`
Expected: All pass

**Step 3: Commit**

```
feat(diagnostics): log external addresses at hole-punch start
```

---

### Task 3: Log Dialing Events During Active Hole-Punch

Promote `Dialing` and `OutgoingConnectionError` events to `info` level when they occur during an active hole-punch attempt, so we can see what addresses DCUtR is dialing and what errors it gets.

**Files:**
- Modify: `cli/src/node/mod.rs` (event loop, around line 199)

**Step 1: Add dialing/error logging in the swarm event handler**

Inside the `event = swarm.select_next_some()` arm of the main loop, add handlers for `Dialing` and enhance `OutgoingConnectionError` when hole-punch is active. Add these after the existing `ConnectionClosed` handler (after line 220) and before the `NewExternalAddrCandidate` handler (line 256):

```rust
if let SwarmEvent::Dialing { peer_id: Some(peer_id), .. } = &event
    && holepunch_deadlines.contains_key(peer_id)
{
    tracing::info!(
        peer = %peer_id,
        "hole-punch dial attempt in progress"
    );
}

if let SwarmEvent::OutgoingConnectionError { peer_id: Some(peer_id), error, .. } = &event
    && holepunch_deadlines.contains_key(peer_id)
{
    tracing::info!(
        peer = %peer_id,
        error = %error,
        "hole-punch dial attempt failed"
    );
}
```

**Step 2: Run tests**

Run: `cargo test -p cli --lib`
Expected: All pass

**Step 3: Commit**

```
feat(diagnostics): log dial attempts and errors during active hole-punch
```

---

### Task 4: Add NatFiltering Enum and NatClassification Struct

Add the new types that will represent the filtering probe result.

**Files:**
- Modify: `cli/src/nat_probe.rs` (add types after `NatMapping`)

**Step 1: Write the failing test**

Add to the `tests` module in `cli/src/nat_probe.rs`:

```rust
#[test]
fn endpoint_independent_filtering_is_full_cone() {
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

proptest! {
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
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p cli --lib nat_probe`
Expected: FAIL — `NatFiltering` and `NatClassification` not defined

**Step 3: Write minimal implementation**

Add after the `NatMapping` impl block (after line 34) in `cli/src/nat_probe.rs`:

```rust
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
```

**Step 4: Run test to verify it passes**

Run: `cargo test -p cli --lib nat_probe`
Expected: All pass

**Step 5: Commit**

```
feat(nat_probe): add NatFiltering enum and NatClassification struct
```

---

### Task 5: Add NAT Probe Protocol Definition

Define the stream protocol ID and serde message types for the filtering probe.

**Files:**
- Modify: `cli/src/protocol.rs` (add protocol constant + constructor)
- Create: `cli/src/nat_probe_protocol.rs` (wire format messages)
- Modify: `cli/src/lib.rs` (add module)

**Step 1: Add protocol constant**

In `cli/src/protocol.rs`, add after line 8:

```rust
pub const NAT_PROBE_PROTOCOL: &str = "/punchgate/nat-probe/1.0.0";
```

Add a constructor after the `tunnel_protocol` function:

```rust
pub fn nat_probe_protocol() -> StreamProtocol {
    // Infallible: NAT_PROBE_PROTOCOL is a compile-time constant starting with '/' as required by StreamProtocol
    StreamProtocol::try_from_owned(NAT_PROBE_PROTOCOL.to_string())
        .expect("NAT_PROBE_PROTOCOL is a valid compile-time constant protocol string")
}
```

**Step 2: Create wire format module**

Create `cli/src/nat_probe_protocol.rs`:

```rust
use std::net::SocketAddr;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NatProbeRequest {
    pub mapped_addr: SocketAddr,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NatProbeResponse {
    pub probe_sent: bool,
}
```

**Step 3: Register module**

In `cli/src/lib.rs`, add after line 4:

```rust
pub mod nat_probe_protocol;
```

**Step 4: Run tests**

Run: `cargo test -p cli --lib`
Expected: All pass

**Step 5: Commit**

```
feat(protocol): add NAT probe stream protocol and wire format
```

---

### Task 6: Implement Filtering Probe — NATed Peer Side

The NATed peer binds a fresh UDP socket, sends STUN to learn its mapped address, then asks the bootstrap to send a probe to that address.

**Files:**
- Modify: `cli/src/nat_probe.rs` (add `probe_filtering` function)

**Step 1: Write the filtering probe function**

Add to `cli/src/nat_probe.rs`, after the `detect_nat_mapping` function:

```rust
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

    // Step 1: Bind fresh UDP socket + STUN to learn mapped address
    let socket = UdpSocket::bind("0.0.0.0:0")
        .await
        .context("failed to bind probe UDP socket")?;

    let mapped_addr = stun_binding_request(&socket, stun_server).await?;
    tracing::info!(%mapped_addr, "filtering probe: learned mapped address via STUN");

    // Step 2: Ask bootstrap to send UDP probe to our mapped address
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

    // Read response
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await?;
    let resp_len = usize::try_from(u32::from_be_bytes(len_buf))
        .expect("u32 fits in usize on all supported platforms");
    anyhow::ensure!(resp_len <= 1024, "probe response too large: {resp_len}");
    let mut resp_buf = vec![0u8; resp_len];
    stream.read_exact(&mut resp_buf).await?;

    let response: crate::nat_probe_protocol::NatProbeResponse = serde_json::from_slice(&resp_buf)?;
    anyhow::ensure!(response.probe_sent, "bootstrap refused to send probe");

    // Step 3: Wait for UDP probe packet to arrive
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
```

**Step 2: Run tests**

Run: `cargo test -p cli --lib`
Expected: All pass (function exists but isn't called yet)

**Step 3: Commit**

```
feat(nat_probe): implement client-side filtering probe via bootstrap
```

---

### Task 7: Implement Filtering Probe — Bootstrap Side

The bootstrap handles incoming NAT probe stream requests by sending a UDP packet to the requested address.

**Files:**
- Modify: `cli/src/node/mod.rs` (register probe handler, spawn accept loop)

**Step 1: Add probe handler function**

Add a new function in `cli/src/node/mod.rs`, before the `extract_peer_id` helper:

```rust
async fn nat_probe_accept_loop(mut incoming: libp2p_stream::IncomingStreams) {
    use futures::{StreamExt, io::{AsyncReadExt, AsyncWriteExt}};

    while let Some((peer_id, mut stream)) = incoming.next().await {
        tokio::spawn(async move {
            match handle_nat_probe(peer_id, &mut stream).await {
                Ok(()) => {}
                Err(e) => tracing::warn!(%peer_id, error = %e, "NAT probe handler failed"),
            }
        });
    }
}

async fn handle_nat_probe(
    peer_id: libp2p::PeerId,
    stream: &mut libp2p::Stream,
) -> anyhow::Result<()> {
    use futures::io::{AsyncReadExt, AsyncWriteExt};

    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await?;
    let len = usize::try_from(u32::from_be_bytes(len_buf))
        .expect("u32 fits in usize on all supported platforms");
    anyhow::ensure!(len <= 1024, "probe request too large: {len}");
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf).await?;

    let request: crate::nat_probe_protocol::NatProbeRequest = serde_json::from_slice(&buf)?;
    tracing::info!(%peer_id, target = %request.mapped_addr, "sending NAT filtering probe");

    let probe_socket = tokio::net::UdpSocket::bind("0.0.0.0:0").await?;
    let probe_data = b"punchgate-nat-probe";
    probe_socket.send_to(probe_data, request.mapped_addr).await?;

    let response = crate::nat_probe_protocol::NatProbeResponse { probe_sent: true };
    let data = serde_json::to_vec(&response)?;
    let len = u32::try_from(data.len()).context("probe response too large")?;
    stream.write_all(&len.to_be_bytes()).await?;
    stream.write_all(&data).await?;
    stream.flush().await?;

    Ok(())
}
```

**Step 2: Register the probe protocol on the bootstrap**

In the `run` function, after the tunnel protocol registration (after line 115), add:

```rust
// Bootstrap nodes accept NAT filtering probe requests
if config.bootstrap_addrs.is_empty() {
    let mut probe_control = swarm.behaviour().stream.new_control();
    let probe_incoming = probe_control
        .accept(protocol::nat_probe_protocol())
        .map_err(|_| anyhow::anyhow!("NAT probe protocol already registered"))?;
    tokio::spawn(nat_probe_accept_loop(probe_incoming));
}
```

**Step 3: Run tests**

Run: `cargo test -p cli --lib`
Expected: All pass

**Step 4: Commit**

```
feat(node): bootstrap handles NAT filtering probe requests
```

---

### Task 8: Integrate Filtering Probe into Startup

NATed peers run the filtering probe after connecting to bootstrap and learning their mapping type.

**Files:**
- Modify: `cli/src/node/mod.rs` (add probe call after relay reservation)

**Step 1: Run filtering probe after relay connection**

In the main event loop, after the relay reservation is accepted and logged (inside the `Event::RelayReservationAccepted` match arm around line 310-311), spawn the filtering probe. We need the stream control for this, so clone it before the loop.

First, after `let mut stream_control = swarm.behaviour().stream.new_control();` (line 111), add:

```rust
let probe_control = swarm.behaviour().stream.new_control();
```

Then, in the event processing loop, add a new match arm after `Event::RelayReservationAccepted` (around line 310-311):

```rust
Event::RelayReservationAccepted { relay_peer } => {
    tracing::info!(%relay_peer, "relay reservation accepted");
    // Spawn filtering probe after relay is established
    if nat_mapping == crate::nat_probe::NatMapping::EndpointIndependent {
        let mut probe_ctl = probe_control.clone();
        let bootstrap = *relay_peer;
        tokio::spawn(async move {
            let filtering = crate::nat_probe::probe_filtering(
                probe_ctl,
                bootstrap,
                crate::nat_probe::DEFAULT_STUN_SERVERS
                    .first()
                    .expect("at least one STUN server configured"),
            ).await;
            match filtering {
                crate::nat_probe::NatFiltering::EndpointIndependent => {
                    tracing::info!(
                        nat_filtering = %filtering,
                        "full cone NAT — hole-punching should succeed"
                    );
                }
                crate::nat_probe::NatFiltering::Restricted => {
                    tracing::warn!(
                        nat_filtering = %filtering,
                        "restricted filtering NAT — hole-punching depends on dial simultaneity"
                    );
                }
                crate::nat_probe::NatFiltering::Unknown => {
                    tracing::warn!(
                        nat_filtering = %filtering,
                        "could not determine NAT filtering type"
                    );
                }
            }
        });
    }
}
```

Note: The relay_peer match arm already exists. Replace the body of the existing `Event::RelayReservationAccepted` match arm with the expanded version above.

**Step 2: Run tests**

Run: `cargo test -p cli --lib`
Expected: All pass

Run: `cargo build --release`
Expected: Compiles successfully

**Step 3: Commit**

```
feat(node): run NAT filtering probe after relay reservation
```

---

### Task 9: Add Timing Correlation Logs

Add structured timestamps at hole-punch lifecycle events so cross-peer logs can be compared.

**Files:**
- Modify: `cli/src/node/mod.rs` (hole-punch start + result timestamps)

**Step 1: Add timestamp to hole-punch start**

In the event loop, where `holepunch_deadlines` is populated (around line 346-350), add a timestamp log:

```rust
for cmd in &commands {
    if let Command::AwaitHolePunch { peer } = cmd {
        holepunch_deadlines.entry(*peer)
            .or_insert(tokio::time::Instant::now() + HOLEPUNCH_TIMEOUT);
        tracing::info!(
            %peer,
            timeout_secs = HOLEPUNCH_TIMEOUT.as_secs(),
            "hole-punch timer started"
        );
    }
}
```

**Step 2: Run tests**

Run: `cargo test -p cli --lib`
Expected: All pass

**Step 3: Commit**

```
feat(diagnostics): add timing correlation logs for hole-punch lifecycle
```

---

### Task 10: Manual 3-Peer Deployment Test

Deploy the modified code to all 3 machines and collect diagnostic logs.

**Step 1: Build and deploy**

```bash
cargo build --release
```

Copy the binary to all 3 machines.

**Step 2: Run all 3 peers**

Run bootstrap, workhorse, and client as before. Use `RUST_LOG=cli=info` (default).

**Step 3: Analyze new diagnostic output**

Look for these new log lines in the NATed peers' output:

1. `nat_filtering=Restricted` or `nat_filtering=Endpoint-Independent` — tells you the filtering type
2. `awaiting hole-punch — external address snapshot` with the `addrs` field — confirms what DCUtR has
3. `hole-punch dial attempt in progress` / `hole-punch dial attempt failed` — shows what DCUtR tried
4. `hole-punch timer started` — timing reference for cross-peer correlation

**Step 4: Determine fix strategy based on results**

| Finding | Action |
|---|---|
| Both peers show `EndpointIndependent` filtering | Bug in address exchange — check address audit logs |
| One/both peers show `Restricted` filtering + good simultaneity | NAT filtering is the root cause — consider hairpin/port-prediction retry |
| `Restricted` filtering + poor simultaneity (>2s skew) | Improve DCUtR relay coordination timing |
| Address audit shows wrong/missing addresses | Fix the external address candidate pipeline |
| `Unknown` filtering (probe failed) | Check bootstrap UDP connectivity, firewall rules |

---

### Summary of All Changes

| Task | Type | Risk | Files |
|---|---|---|---|
| 1. Bump timeout | Constant change | None | `protocol.rs` |
| 2. Log addresses at hole-punch start | Logging | None | `node/execute.rs` |
| 3. Log dial events during hole-punch | Logging | None | `node/mod.rs` |
| 4. NatFiltering + NatClassification types | New types + tests | None | `nat_probe.rs` |
| 5. NAT probe protocol definition | Protocol + wire format | None | `protocol.rs`, `nat_probe_protocol.rs`, `lib.rs` |
| 6. Filtering probe — client side | New function | Low | `nat_probe.rs` |
| 7. Filtering probe — bootstrap side | New handler | Low | `node/mod.rs` |
| 8. Integrate probe into startup | Integration | Low | `node/mod.rs` |
| 9. Timing correlation logs | Logging | None | `node/mod.rs` |
| 10. Manual deployment test | Manual | None | N/A |
