# Codebase Simplification: Surgical Removal of NAT Priming — Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Remove NAT priming infrastructure, enable AutoNAT by default, and simplify the state machine so DCUtR is the sole hole-punching strategy.

**Architecture:** Remove 3 commands, 2 events, and ~300 lines of priming/filtering/retry code. Keep `relayed_peers` (needed for relay-aware routing beyond priming). Move hole-punch timer trigger from command-scanning to event-based. Set `nat_mapping` directly at startup instead of via synthetic event.

**Tech Stack:** Rust, libp2p 0.56, tokio, proptest

**Design deviation:** The design doc lists `relayed_peers` for removal, characterizing it as "for priming decisions." In fact, `relayed_peers` is fundamental to `route_connected_peer()` which decides whether to await hole-punch vs spawn directly — it MUST stay. Only `peer_external_addrs` is priming-only and gets removed.

---

### Task 1: Remove PrimeNatMapping + PrimeAndDialDirect commands

**Files:**
- Modify: `cli/src/state/command.rs:46-53`
- Modify: `cli/src/node/execute.rs:125-149`

**Step 1: Remove command variants**

In `cli/src/state/command.rs`, delete lines 46-53 (the two priming variants):

```rust
// DELETE these lines:
    PrimeNatMapping {
        peer: PeerId,
        peer_addrs: Vec<Multiaddr>,
    },
    PrimeAndDialDirect {
        peer: PeerId,
        peer_addrs: Vec<Multiaddr>,
    },
```

**Step 2: Remove execute.rs handlers**

In `cli/src/node/execute.rs`, delete lines 125-149 (the `PrimeNatMapping | PrimeAndDialDirect` match arm):

```rust
// DELETE this entire match arm:
            Command::PrimeNatMapping { peer, peer_addrs }
            | Command::PrimeAndDialDirect { peer, peer_addrs } => {
                // ... all 24 lines
            }
```

**Step 3: Verify compilation**

Run: `cargo check -p cli 2>&1 | head -50`
Expected: Compilation errors in `tunnel.rs` (still constructs removed variants). These are fixed in Task 2.

---

### Task 2: Remove priming from TunnelState

**Files:**
- Modify: `cli/src/state/tunnel.rs:19,22,38,41,288,300-309,315-331` and tests

**Step 1: Remove `peer_external_addrs` field**

In `cli/src/state/tunnel.rs`, remove field from struct (line 22) and from `new()` (line 41):

```rust
// In struct TunnelState (DELETE line 22):
    peer_external_addrs: HashMap<PeerId, Vec<Multiaddr>>,

// In new() (DELETE line 41):
            peer_external_addrs: HashMap::new(),
```

**Step 2: Remove `HolePunchRetryTick` handler**

Replace the handler at lines 300-309 with a noop by moving `HolePunchRetryTick` to the noop match arm (lines 333-347). Since the event variant is removed in Task 3, just delete lines 300-309 entirely.

```rust
// DELETE lines 300-309:
            Event::HolePunchRetryTick { peer } => {
                if self.relayed_peers.contains(&peer) {
                    let peer_addrs = self
                        .peer_external_addrs
                        .get(&peer)
                        .cloned()
                        .unwrap_or_default();
                    commands.push(Command::PrimeAndDialDirect { peer, peer_addrs });
                }
            }
```

**Step 3: Remove `NatMappingDetected` handler**

Delete lines 311-313:

```rust
// DELETE:
            Event::NatMappingDetected(mapping) => {
                self.nat_mapping = mapping;
            }
```

**Step 4: Remove `PeerIdentified` priming logic**

Replace the `PeerIdentified` handler (lines 315-331) with a noop. Move `PeerIdentified` to the noop match arm:

```rust
// DELETE lines 315-331 and add PeerIdentified to the noop arm:
            | Event::PeerIdentified { .. }
```

**Step 5: Clean up `ConnectionLost` handler**

Remove `peer_external_addrs.remove(&peer)` from lines 282-298 (line 288):

```rust
// DELETE line 288:
                self.peer_external_addrs.remove(&peer);
```

**Step 6: Remove `Multiaddr` import if unused**

Check if `Multiaddr` is still used in tunnel.rs after removing `peer_external_addrs`. The `use libp2p::{Multiaddr, PeerId};` on line 3 — `Multiaddr` may be unused. Remove it if compiler warns.

**Step 7: Remove priming-related tests**

Delete these test functions entirely:

- `holepunch_retry_dials_relayed_peer` (lines 852-863)
- `holepunch_retry_noop_for_non_relayed_peer` (lines 865-874)
- `tunnel_connected_relayed_no_prime_deferred_to_peer_identified` (lines 956-974)
- `peer_identified_primes_relayed_peer` (lines 976-997)
- `peer_identified_no_prime_for_non_relayed_peer` (lines 999-1016)
- `peer_identified_no_prime_when_symmetric_nat` (lines 1018-1036)
- `peer_identified_no_prime_with_empty_addrs` (lines 1038-1055)
- `nat_mapping_detected_updates_tunnel_state` (lines 1057-1069)

**Step 8: Update `connection_lost_cleans_tracking_state` test**

Remove the `peer_external_addrs` line from this test (line 758):

```rust
// DELETE:
            state.peer_external_addrs.insert(peer, vec![]);
// DELETE assertion:
            prop_assert!(!state.peer_external_addrs.contains_key(&peer));
```

**Step 9: Verify compilation**

Run: `cargo check -p cli 2>&1 | head -50`
Expected: Errors from event variants still referenced. Fixed in Task 3.

---

### Task 3: Remove HolePunchRetryTick + NatMappingDetected events

**Files:**
- Modify: `cli/src/state/event.rs:33,82-84`

**Step 1: Remove event variants**

Delete from `cli/src/state/event.rs`:

```rust
// DELETE line 33:
    NatMappingDetected(crate::nat_probe::NatMapping),

// DELETE lines 82-84:
    HolePunchRetryTick {
        peer: PeerId,
    },
```

**Step 2: Verify compilation**

Run: `cargo check -p cli 2>&1 | head -50`
Expected: Errors in `node/mod.rs` where these events are constructed. Fixed in Task 4.

---

### Task 4: Remove AwaitHolePunch command + move timer trigger

**Files:**
- Modify: `cli/src/state/command.rs:43-45`
- Modify: `cli/src/node/execute.rs:117-124`
- Modify: `cli/src/state/tunnel.rs` (route_connected_peer + TunnelPeerConnected handler)
- Modify: `cli/src/node/mod.rs:452-462`

**Step 1: Remove AwaitHolePunch from command.rs**

Delete from `cli/src/state/command.rs` lines 43-45:

```rust
// DELETE:
    AwaitHolePunch {
        peer: PeerId,
    },
```

**Step 2: Remove AwaitHolePunch handler from execute.rs**

Delete lines 117-124:

```rust
// DELETE:
            Command::AwaitHolePunch { peer } => {
                let external_addrs: Vec<_> = swarm.external_addresses().collect();
                tracing::info!(
                    %peer,
                    addrs = ?external_addrs,
                    "awaiting hole-punch — external address snapshot"
                );
            }
```

**Step 3: Update route_connected_peer in tunnel.rs**

Replace the `relayed_peers` branch that emits `AwaitHolePunch` (lines 97-102) — stop emitting the command, just track state:

```rust
// BEFORE (lines 87-113):
        if self.holepunch_failed.contains(&peer) || !self.nat_mapping.is_holepunch_viable() {
            specs
                .into_iter()
                .map(|(service, bind)| Command::SpawnTunnel {
                    peer,
                    service,
                    bind,
                    relayed: true,
                })
                .collect()
        } else if self.relayed_peers.contains(&peer) {
            self.awaiting_holepunch
                .entry(peer)
                .or_default()
                .extend(specs);
            vec![Command::AwaitHolePunch { peer }]
        } else {
            // ...
        }

// AFTER:
        if self.holepunch_failed.contains(&peer) || !self.nat_mapping.is_holepunch_viable() {
            specs
                .into_iter()
                .map(|(service, bind)| Command::SpawnTunnel {
                    peer,
                    service,
                    bind,
                    relayed: true,
                })
                .collect()
        } else if self.relayed_peers.contains(&peer) {
            self.awaiting_holepunch
                .entry(peer)
                .or_default()
                .extend(specs);
            vec![]
        } else {
            // ... unchanged
        }
```

**Step 4: Update TunnelPeerConnected relayed handler in tunnel.rs**

Remove `AwaitHolePunch` emission from lines 188-192:

```rust
// BEFORE (lines 186-202):
                        if let Some(specs) = self.pending_by_peer.remove(&peer) {
                            if self.nat_mapping.is_holepunch_viable() {
                                self.awaiting_holepunch
                                    .entry(peer)
                                    .or_default()
                                    .extend(specs);
                                commands.push(Command::AwaitHolePunch { peer });
                            } else {
                                // ... relay spawn
                            }
                        }

// AFTER:
                        if let Some(specs) = self.pending_by_peer.remove(&peer) {
                            if self.nat_mapping.is_holepunch_viable() {
                                self.awaiting_holepunch
                                    .entry(peer)
                                    .or_default()
                                    .extend(specs);
                            } else {
                                // ... relay spawn (unchanged)
                            }
                        }
```

**Step 5: Move timer trigger in node/mod.rs**

Replace the AwaitHolePunch command-scanning loop (lines 452-462) with event-based timer start. In the event processing section, add after the existing match block (around line 396):

```rust
// REPLACE lines 452-462:
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

// WITH (add to the event match block near line 434):
                        Event::TunnelPeerConnected { peer, relayed: true } => {
                            holepunch_deadlines.entry(*peer)
                                .or_insert(tokio::time::Instant::now() + HOLEPUNCH_TIMEOUT);
                            tracing::info!(
                                %peer,
                                timeout_secs = HOLEPUNCH_TIMEOUT.as_secs(),
                                "hole-punch timer started"
                            );
                        }
```

**Step 6: Update tests that assert AwaitHolePunch**

In `cli/src/state/tunnel.rs` tests, update these to assert on state instead of commands:

`tunnel_peer_connected_relayed_waits_for_holepunch`:
```rust
// BEFORE:
            let has_await = commands.contains(&Command::AwaitHolePunch { peer });
            prop_assert!(has_await);

// AFTER:
            prop_assert!(commands.is_empty());
```

`dht_peer_lookup_relayed_awaits_holepunch`:
```rust
// BEFORE:
            let has_await = commands.contains(&Command::AwaitHolePunch { peer });
            prop_assert!(has_await);

// AFTER:
            prop_assert!(commands.is_empty());
```

`dht_service_resolved_relayed_awaits_holepunch`:
```rust
// BEFORE:
            let has_await = commands.contains(&Command::AwaitHolePunch { peer: provider });
            prop_assert!(has_await);

// AFTER:
            prop_assert!(commands.is_empty());
```

`relayed_peer_awaits_holepunch_when_cone_nat`:
```rust
// BEFORE:
            let has_await = commands.contains(&Command::AwaitHolePunch { peer });
            prop_assert!(has_await);

// AFTER:
            prop_assert!(commands.is_empty());
```

**Step 7: Verify compilation**

Run: `cargo check -p cli 2>&1 | head -50`
Expected: PASS (or errors in node/mod.rs from removed events, fixed in Task 5)

---

### Task 5: Clean up node/mod.rs — remove retry, probe, and synthetic injection

**Files:**
- Modify: `cli/src/node/mod.rs`

**Step 1: Remove imports for removed items**

Update the import block (lines 25-28):

```rust
// BEFORE:
    protocol::{
        self, DISCOVERY_TIMEOUT, HOLEPUNCH_RETRY_INTERVAL, HOLEPUNCH_TIMEOUT,
        IDLE_CONNECTION_TIMEOUT,
    },

// AFTER:
    protocol::{self, DISCOVERY_TIMEOUT, HOLEPUNCH_TIMEOUT, IDLE_CONNECTION_TIMEOUT},
```

**Step 2: Set nat_mapping directly on TunnelState**

After `tunnel_state` creation (line 126), add direct setter. Then remove the synthetic injections (lines 171-184):

```rust
// ADD after line 126:
    tunnel_state.set_nat_mapping(nat_mapping);

// DELETE lines 171-184 (synthetic NatStatus::Private + NatMappingDetected injections):
    if !config.bootstrap_addrs.is_empty() {
        let old_phase = app_state.phase();
        let (new_state, commands) =
            app_state.transition(Event::NatStatusChanged(NatStatus::Private));
        log_phase_transition(old_phase, new_state.phase(), commands.len());
        app_state = new_state;
        execute_commands(&mut swarm, &commands, &config.exposed, &mut ctx);

        let old_phase = app_state.phase();
        let (new_state, commands) = app_state.transition(Event::NatMappingDetected(nat_mapping));
        log_phase_transition(old_phase, new_state.phase(), commands.len());
        app_state = new_state;
        execute_commands(&mut swarm, &commands, &config.exposed, &mut ctx);
    }
```

**Step 3: Remove NAT probe protocol handler**

Delete lines 117-123 (probe protocol registration on bootstrap):

```rust
// DELETE:
    if config.bootstrap_addrs.is_empty() {
        let mut probe_control = swarm.behaviour().stream.new_control();
        let probe_incoming = probe_control
            .accept(protocol::nat_probe_protocol())
            .map_err(|_| anyhow::anyhow!("NAT probe protocol already registered"))?;
        tokio::spawn(nat_probe_accept_loop(probe_incoming));
    }
```

**Step 4: Remove `filtering_probe_control`**

Delete line 138:

```rust
// DELETE:
    let filtering_probe_control = swarm.behaviour().stream.new_control();
```

**Step 5: Remove `holepunch_retries` HashMap**

Delete line 196:

```rust
// DELETE:
    let mut holepunch_retries: HashMap<PeerId, tokio::time::Instant> = HashMap::new();
```

**Step 6: Remove `next_holepunch_retry` computation**

Delete line 210:

```rust
// DELETE:
            let next_holepunch_retry = holepunch_retries.values().min().copied();
```

**Step 7: Remove filtering probe spawn**

Delete the filtering probe spawn in RelayReservationAccepted handler (lines 399-416):

```rust
// DELETE:
                            if nat_mapping == crate::nat_probe::NatMapping::EndpointIndependent
                                && let Some(&stun_server) = crate::nat_probe::DEFAULT_STUN_SERVERS.first()
                            {
                                let probe_ctl = filtering_probe_control.clone();
                                let bootstrap = *relay_peer;
                                tokio::spawn(async move {
                                    let filtering = crate::nat_probe::probe_filtering(
                                        probe_ctl,
                                        bootstrap,
                                        stun_server,
                                    ).await;
                                    tracing::info!(
                                        %filtering,
                                        %bootstrap,
                                        "NAT filtering probe result"
                                    );
                                });
                            }
```

**Step 8: Remove retry entries from HolePunchFailed handler**

In the HolePunchFailed handler (lines 423-432), remove the retry scheduling:

```rust
// DELETE lines 431-432:
                            holepunch_retries.entry(*remote_peer)
                                .or_insert(tokio::time::Instant::now() + HOLEPUNCH_RETRY_INTERVAL);
```

**Step 9: Remove retry cleanup from TunnelPeerConnected + ConnectionLost**

```rust
// DELETE from TunnelPeerConnected (line 436):
                            holepunch_retries.remove(peer);

// DELETE from ConnectionLost (line 440):
                            holepunch_retries.remove(peer);
```

**Step 10: Remove retry cleanup from HolePunchSucceeded**

```rust
// DELETE from HolePunchSucceeded (line 421):
                            holepunch_retries.remove(remote_peer);
```

**Step 11: Remove holepunch retry scheduling from timeout arm**

In the holepunch timeout select arm (lines 549-550):

```rust
// DELETE:
                    holepunch_retries.entry(peer)
                        .or_insert(tokio::time::Instant::now() + HOLEPUNCH_RETRY_INTERVAL);
```

**Step 12: Remove entire holepunch retry select arm**

Delete lines 553-580 (the third timer select arm):

```rust
// DELETE entire arm:
            _ = async {
                match next_holepunch_retry {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending::<()>().await,
                }
            } => {
                // ... all retry logic
            }
```

**Step 13: Remove `nat_probe_accept_loop` and `handle_nat_probe` functions**

Delete lines 608-660:

```rust
// DELETE: nat_probe_accept_loop (lines 608-617)
// DELETE: handle_nat_probe (lines 619-660)
```

**Step 14: Remove unused imports**

After all removals, clean up any unused imports. Likely candidates:
- `NatStatus` from state import (check if still used for port mapping NatStatusChanged)
- `Command` (check if still used for the AwaitHolePunch scanning — removed in Task 4)

Port mapping still uses `Event::NatStatusChanged(NatStatus::Public)` so `NatStatus` stays. `Command` is no longer directly matched in the event loop, so remove from import if unused.

**Step 15: Verify compilation**

Run: `cargo check -p cli 2>&1 | head -50`
Expected: PASS (or warnings about unused imports, fix those)

---

### Task 6: Simplify protocol.rs

**Files:**
- Modify: `cli/src/protocol.rs`

**Step 1: Remove constants and function**

Delete from `cli/src/protocol.rs`:

```rust
// DELETE line 8:
pub const NAT_PROBE_PROTOCOL: &str = "/punchgate/nat-probe/1.0.0";

// DELETE line 16:
pub const HOLEPUNCH_RETRY_INTERVAL: Duration = Duration::from_secs(60);

// DELETE lines 30-34:
pub fn nat_probe_protocol() -> StreamProtocol {
    // Infallible: NAT_PROBE_PROTOCOL is a compile-time constant starting with '/' as required by StreamProtocol
    StreamProtocol::try_from_owned(NAT_PROBE_PROTOCOL.to_string())
        .expect("NAT_PROBE_PROTOCOL is a valid compile-time constant protocol string")
}
```

**Step 2: Change HOLEPUNCH_TIMEOUT**

```rust
// BEFORE (line 14):
pub const HOLEPUNCH_TIMEOUT: Duration = Duration::from_secs(30);

// AFTER:
pub const HOLEPUNCH_TIMEOUT: Duration = Duration::from_secs(15);
```

**Step 3: Remove unused import if StreamProtocol only used in remaining functions**

Check if `StreamProtocol` is still needed (yes, for `kad_protocol()` and `tunnel_protocol()`).

**Step 4: Verify compilation**

Run: `cargo check -p cli 2>&1 | head -50`
Expected: PASS

---

### Task 7: Simplify nat_probe.rs — remove filtering

**Files:**
- Modify: `cli/src/nat_probe.rs`

**Step 1: Remove NatFiltering enum**

Delete lines 36-44:

```rust
// DELETE:
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, strum::Display)]
pub enum NatFiltering {
    #[strum(serialize = "Endpoint-Independent")]
    EndpointIndependent,
    #[strum(serialize = "Restricted")]
    Restricted,
    #[strum(serialize = "Unknown")]
    Unknown,
}
```

**Step 2: Remove NatClassification struct**

Delete lines 46-56:

```rust
// DELETE:
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

**Step 3: Remove probe_filtering functions**

Delete lines 72-140 (`probe_filtering` + `probe_filtering_inner`):

```rust
// DELETE: pub async fn probe_filtering(...) → NatFiltering
// DELETE: async fn probe_filtering_inner(...) → Result<NatFiltering>
```

**Step 4: Remove unused imports**

After removing the filtering functions, these imports may be unused:
- `libp2p_stream::Control` (used only in probe_filtering)
- `libp2p::PeerId` (used only in probe_filtering)
- `futures::io::{AsyncReadExt, AsyncWriteExt}` (used only in probe_filtering_inner)

Check and remove as needed. The `crate::nat_probe_protocol` import will also be unused (deleted in Task 8).

**Step 5: Remove filtering-related tests**

Delete these test functions:

- `classification_holepunch_viable_requires_ei_mapping` (lines 323-335)
- `classification_full_cone_always_viable` (lines 337-349)
- `endpoint_independent_filtering_display` (lines 352-358)
- `restricted_filtering_display` (lines 360-363)
- `classification_combines_mapping_and_filtering` (lines 365-373)

**Step 6: Verify compilation**

Run: `cargo check -p cli 2>&1 | head -50`
Expected: PASS

---

### Task 8: Delete nat_probe_protocol.rs + update lib.rs

**Files:**
- Delete: `cli/src/nat_probe_protocol.rs`
- Modify: `cli/src/lib.rs:5`

**Step 1: Delete the file**

```bash
rm cli/src/nat_probe_protocol.rs
```

**Step 2: Remove module declaration**

In `cli/src/lib.rs`, delete line 5:

```rust
// DELETE:
pub mod nat_probe_protocol;
```

**Step 3: Verify compilation**

Run: `cargo check -p cli 2>&1 | head -50`
Expected: PASS

---

### Task 9: Enable AutoNAT by default

**Files:**
- Modify: `cli/Cargo.toml:7`

**Step 1: Change default features**

```toml
# BEFORE (line 7):
default = []

# AFTER:
default = ["autonat"]
```

**Step 2: Verify compilation**

Run: `cargo check -p cli 2>&1 | head -50`
Expected: PASS

---

### Task 10: Run full test suite

**Step 1: Run all tests**

Run: `cargo test -p cli 2>&1`
Expected: All tests pass. Estimated ~100 tests remaining after removals.

**Step 2: Run clippy**

Run: `cargo clippy -p cli -- -D warnings 2>&1 | head -50`
Expected: No warnings.

**Step 3: Count removals**

Run: `git diff --stat`
Expected: ~300-400 lines removed, ~10 lines changed, 1 file deleted.

---

### Task 11: Commit

**Step 1: Stage and commit**

```bash
git add cli/src/state/command.rs cli/src/state/event.rs cli/src/state/tunnel.rs \
       cli/src/node/execute.rs cli/src/node/mod.rs \
       cli/src/protocol.rs cli/src/nat_probe.rs \
       cli/src/lib.rs cli/Cargo.toml Cargo.lock
git add --update cli/src/nat_probe_protocol.rs
git commit -m "refactor(cli): remove NAT priming, enable AutoNAT, simplify state machine

Remove 3 commands (PrimeNatMapping, PrimeAndDialDirect, AwaitHolePunch),
2 events (HolePunchRetryTick, NatMappingDetected), NAT filtering probe,
and retry timer infrastructure. DCUtR is now the sole hole-punching
strategy with a 15s timeout before relay fallback.

Enable AutoNAT by default, replacing synthetic NatStatus::Private injection
with real NAT reachability probing through bootstrap peers."
```
