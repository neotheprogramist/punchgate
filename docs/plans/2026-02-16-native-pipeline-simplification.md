# Native Pipeline Simplification — Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Remove broken manual address management, flatten Phase state machine to Joining/Ready/ShuttingDown, gate relay on AutoNAT Private status, and let libp2p's native Identify → AutoNAT → DCUtR pipeline handle address discovery.

**Architecture:** Delete `discover_external_ip()` and `make_external_addr()`. Remove `reqwest` dependency. Flatten `Phase` from 4 to 3 variants. Replace `RelayState` enum with `relay_reserved: bool`. Gate relay reservation on `NatStatus::Private`. Bootstrap server (no `--bootstrap` peers) confirms addresses via Identify's `observed_addr`.

**Tech Stack:** Rust, libp2p 0.56, tokio, proptest

**Design doc:** `docs/plans/2026-02-16-native-pipeline-simplification-design.md`

---

### Task 1: Flatten Phase enum and update test generators

**Files:**
- Modify: `cli/src/state/peer.rs:31-37` (Phase enum)
- Modify: `cli/src/test_utils.rs:77-84` (arb_phase generator)
- Modify: `cli/src/state/mod.rs:10` (re-export, remove RelayState)

**Step 1: Update Phase enum**

In `cli/src/state/peer.rs`, replace:
```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, strum::Display)]
pub enum Phase {
    Initializing,
    Discovering,
    Participating,
    ShuttingDown,
}
```
with:
```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, strum::Display)]
pub enum Phase {
    Joining,
    Ready,
    ShuttingDown,
}
```

**Step 2: Replace RelayState enum with bool**

In `cli/src/state/peer.rs`, delete `RelayState` enum (lines 63-68). In `PeerState` struct, replace `pub relay: RelayState` with `pub relay_reserved: bool`. Update `Default`/`new()` to set `relay_reserved: false`. Update initial `phase` from `Phase::Initializing` to `Phase::Joining`.

**Step 3: Update test generators**

In `cli/src/test_utils.rs`, update `arb_phase()`:
```rust
pub fn arb_phase() -> impl Strategy<Value = Phase> {
    prop_oneof![
        Just(Phase::Joining),
        Just(Phase::Ready),
        Just(Phase::ShuttingDown),
    ]
}
```

Delete `arb_relay_state()` function entirely.

**Step 4: Update re-exports**

In `cli/src/state/mod.rs`, remove `RelayState` from the re-export line:
```rust
pub use peer::{NatStatus, NatStatusParseError, PeerState, Phase};
```

**Step 5: Verify compilation fails at expected locations**

Run: `cargo check -p cli 2>&1 | head -60`
Expected: Compilation errors in `peer.rs` transition logic, `tunnel.rs` Phase references, `app.rs` tests, `node/mod.rs`. These will be fixed in subsequent tasks.

**Step 6: Commit**

```bash
git add cli/src/state/peer.rs cli/src/state/mod.rs cli/src/test_utils.rs
git commit -m "refactor(state): flatten Phase to Joining/Ready/ShuttingDown and remove RelayState"
```

---

### Task 2: Rewrite PeerState transition logic

**Files:**
- Modify: `cli/src/state/peer.rs:87-337` (PeerState impl + MealyMachine impl)

**Step 1: Delete `maybe_enter_participating` and rewrite with `maybe_become_ready`**

Replace the entire `maybe_enter_participating` method with:
```rust
fn maybe_become_ready(mut self) -> (Self, Vec<Command>) {
    match (self.phase, self.kad_bootstrapped, self.external_addrs.is_empty()) {
        (Phase::Joining, true, false) => {
            self.phase = Phase::Ready;
            (self, Vec::new())
        }
        (Phase::Joining, ..) | (Phase::Ready, ..) | (Phase::ShuttingDown, ..) => {
            (self, Vec::new())
        }
    }
}
```

**Step 2: Rewrite `transition` method**

Key changes to the match arms:

1. **`ListeningOn`**: Remove `AddExternalAddress` command entirely — native pipeline handles it.
```rust
Event::ListeningOn { .. } => {}
```

2. **`BootstrapConnected`**: Merge Initializing/Discovering into Joining, remove eager relay.
```rust
Event::BootstrapConnected { peer, addr } => {
    self.bootstrap_peers.insert(peer);
    self.known_peers.entry(peer).or_default().insert(addr.clone());
    commands.push(Command::KademliaAddAddress { peer, addr });
    match self.phase {
        Phase::Joining => {
            commands.push(Command::KademliaBootstrap);
        }
        Phase::Ready => {
            commands.push(Command::KademliaBootstrap);
        }
        Phase::ShuttingDown => {}
    }
}
```

3. **`KademliaBootstrapOk`**: Set flag, try ready.
```rust
Event::KademliaBootstrapOk => {
    self.kad_bootstrapped = true;
    let (s, cmds) = self.maybe_become_ready();
    self = s;
    commands.extend(cmds);
}
```

4. **`NatStatusChanged`**: Gate relay on Private + not reserved + has bootstrap peer.
```rust
Event::NatStatusChanged(status) => {
    self.nat_status = status;
    match (status, self.relay_reserved) {
        (NatStatus::Private, false) => {
            let relay_candidate = self.known_peers.iter()
                .find(|(p, _)| self.bootstrap_peers.contains(p))
                .and_then(|(&peer, addrs)| {
                    prefer_non_loopback(addrs).map(|addr| (peer, addr.clone()))
                });
            if let Some((relay_peer, relay_addr)) = relay_candidate {
                commands.push(Command::RequestRelayReservation { relay_peer, relay_addr });
            }
        }
        (NatStatus::Public, true) => {
            self.relay_reserved = false;
        }
        (NatStatus::Public, false)
        | (NatStatus::Private, true)
        | (NatStatus::Unknown, _) => {}
    }
}
```

5. **`DiscoveryTimeout`**: Force transition to Ready if conditions met.
```rust
Event::DiscoveryTimeout => {
    if self.phase == Phase::Joining && self.kad_bootstrapped && !self.known_peers.is_empty() {
        self.phase = Phase::Ready;
    }
}
```

6. **`RelayReservationAccepted`**: Just set bool.
```rust
Event::RelayReservationAccepted { .. } => {
    self.relay_reserved = true;
}
```

7. **`RelayReservationFailed`**: Just clear bool.
```rust
Event::RelayReservationFailed { .. } => {
    self.relay_reserved = false;
}
```

8. **`ExternalAddrConfirmed`**: Insert + try ready.
```rust
Event::ExternalAddrConfirmed { addr } => {
    self.external_addrs.insert(addr);
    let (s, cmds) = self.maybe_become_ready();
    self = s;
    commands.extend(cmds);
}
```

9. **`NoBootstrapPeers`**: Transition directly to Ready (bootstrap server).
```rust
Event::NoBootstrapPeers => match self.phase {
    Phase::Joining => {
        self.phase = Phase::Ready;
    }
    Phase::Ready | Phase::ShuttingDown => {}
},
```

10. **`ConnectionLost`**: Regress Ready → Joining when all peers lost.
```rust
Event::ConnectionLost { peer, remaining_connections } => {
    if remaining_connections == 0 {
        self.known_peers.remove(&peer);
        if self.relay_reserved {
            // Check if lost peer was our relay — simplified: just reset
            self.relay_reserved = false;
        }
        match (self.phase, self.known_peers.is_empty(), self.bootstrap_peers.is_empty()) {
            (Phase::Ready, true, false) => {
                self.phase = Phase::Joining;
                self.kad_bootstrapped = false;
                commands.push(Command::KademliaBootstrap);
            }
            (Phase::Ready, true, true)
            | (Phase::Ready, false, _)
            | (Phase::Joining, ..)
            | (Phase::ShuttingDown, ..) => {}
        }
    }
}
```

11. **`ShutdownRequested`**: Same as before.

12. Remove match arm for events that PeerState doesn't handle — keep the catch-all.

**Step 3: Verify only test compilation errors remain**

Run: `cargo check -p cli --lib 2>&1 | head -60`
Expected: Errors only in test modules and `tunnel.rs` Phase references.

**Step 4: Commit**

```bash
git add cli/src/state/peer.rs
git commit -m "refactor(state): rewrite PeerState transitions for Joining/Ready phases"
```

---

### Task 3: Update TunnelState and AppState for new Phase names

**Files:**
- Modify: `cli/src/state/tunnel.rs:114-120` (PhaseChanged match)
- Modify: `cli/src/state/app.rs` (no changes needed if Phase references are through enum)

**Step 1: Update TunnelState PhaseChanged handler**

In `cli/src/state/tunnel.rs`, change:
```rust
Event::PhaseChanged {
    new: Phase::Participating,
    ..
} => {
```
to:
```rust
Event::PhaseChanged {
    new: Phase::Ready,
    ..
} => {
```

**Step 2: Update TunnelState catch-all for NoBootstrapPeers**

In `cli/src/state/tunnel.rs` line 261, the `NoBootstrapPeers` is in the catch-all. No change needed.

**Step 3: Verify lib compiles (tests may still fail)**

Run: `cargo check -p cli --lib 2>&1 | head -30`
Expected: Clean compilation for lib code.

**Step 4: Commit**

```bash
git add cli/src/state/tunnel.rs
git commit -m "refactor(state): update TunnelState for Phase::Ready"
```

---

### Task 4: Rewrite PeerState property tests

**Files:**
- Modify: `cli/src/state/peer.rs:341-944` (all tests)

**Step 1: Rewrite all tests for new Phase names and relay logic**

Replace the entire test module. Key test changes:

- `bootstrap_enters_discovering` → `bootstrap_starts_kad_in_joining`: Joining stays Joining, emits KademliaBootstrap
- `kad_completion_enters_participating` → `kad_and_external_addr_enter_ready`: Need BOTH kad_bootstrapped AND external_addrs non-empty
- `participating_always_requests_relay` → DELETE (relay is now gated on NatStatus::Private)
- `private_nat_is_noop_for_relay` → `private_nat_requests_relay`: NatStatus::Private now triggers relay
- All `Phase::Initializing` → `Phase::Joining`, `Phase::Discovering` → `Phase::Joining`, `Phase::Participating` → `Phase::Ready`
- `RelayState::Requesting/Reserved/Idle` → `relay_reserved: true/false`
- Add new test: `external_addr_confirmed_triggers_ready` — when Joining + kad_bootstrapped + gets ExternalAddrConfirmed → Ready
- Add new test: `nat_private_requests_relay_when_bootstrap_known`

See design doc for the full set of transition rules to test.

**Step 2: Run tests**

Run: `cargo test -p cli --lib 2>&1 | tail -20`
Expected: All peer state tests pass.

**Step 3: Commit**

```bash
git add cli/src/state/peer.rs
git commit -m "test(state): rewrite PeerState property tests for simplified phases"
```

---

### Task 5: Update AppState and TunnelState tests

**Files:**
- Modify: `cli/src/state/app.rs:57-166` (app tests)
- Modify: `cli/src/state/tunnel.rs:272-736` (tunnel tests)

**Step 1: Update AppState tests**

- `composition_publishes_on_participating_entry` → needs to provide ExternalAddrConfirmed to trigger Ready:
```rust
let (app, _) = app.transition(Event::BootstrapConnected { peer, addr });
let (app, _) = app.transition(Event::ExternalAddrConfirmed { addr: ext_addr });
let (app, commands) = app.transition(Event::KademliaBootstrapOk);
prop_assert_eq!(app.phase(), Phase::Ready);
prop_assert!(commands.contains(&Command::PublishServices));
```
- `composition_publishes_on_relay_accepted` → relay is now gated on NatStatus, adjust setup
- `no_bootstrap_publishes_directly` → uses NoBootstrapPeers → Ready directly
- Update all `Phase::Participating` → `Phase::Ready`, `Phase::Discovering` → `Phase::Joining`
- Remove tests for relay commands in BootstrapConnected (no longer eager)

**Step 2: Update TunnelState tests**

- `phase_changed_to_participating_publishes` → `Phase::Ready`:
```rust
prop_assume!(old != Phase::Ready);
let (_, commands) = state.transition(Event::PhaseChanged {
    old, new: Phase::Ready,
});
```
- All `Phase::Participating` → `Phase::Ready` in test assertions

**Step 3: Run all tests**

Run: `cargo test -p cli 2>&1 | tail -20`
Expected: All tests pass.

**Step 4: Commit**

```bash
git add cli/src/state/app.rs cli/src/state/tunnel.rs
git commit -m "test(state): update AppState and TunnelState tests for simplified phases"
```

---

### Task 6: Add `observed_addr` to PeerIdentified event and update translate.rs

**Files:**
- Modify: `cli/src/state/event.rs:27-30` (PeerIdentified variant)
- Modify: `cli/src/node/translate.rs:163-168` (Identify::Received translation)

**Step 1: Add observed_addr field to PeerIdentified**

In `cli/src/state/event.rs`:
```rust
PeerIdentified {
    peer: PeerId,
    listen_addrs: Vec<Multiaddr>,
    observed_addr: Multiaddr,
},
```

**Step 2: Update translate.rs to pass observed_addr**

In `cli/src/node/translate.rs`:
```rust
BehaviourEvent::Identify(identify::Event::Received { peer_id, info, .. }) => {
    vec![Event::PeerIdentified {
        peer: *peer_id,
        listen_addrs: info.listen_addrs.clone(),
        observed_addr: info.observed_addr.clone(),
    }]
}
```

**Step 3: Update PeerState handler for PeerIdentified**

In `cli/src/state/peer.rs`, the `PeerIdentified` match arm ignores the new field:
```rust
Event::PeerIdentified { peer, listen_addrs, .. } => {
```

**Step 4: Update test that constructs PeerIdentified**

In `cli/src/state/peer.rs` test `peer_identified_adds_all_addresses`, add `observed_addr`:
```rust
let (new_state, commands) = state.transition(Event::PeerIdentified {
    peer,
    listen_addrs: addrs.clone(),
    observed_addr: addrs.first().expect("at least one addr generated").clone(),
});
```

Similarly update any other test constructing `PeerIdentified`.

**Step 5: Run tests**

Run: `cargo test -p cli 2>&1 | tail -20`
Expected: All tests pass.

**Step 6: Commit**

```bash
git add cli/src/state/event.rs cli/src/node/translate.rs cli/src/state/peer.rs
git commit -m "feat(state): add observed_addr to PeerIdentified for bootstrap address confirmation"
```

---

### Task 7: Simplify external_addr.rs — delete manual address management

**Files:**
- Modify: `cli/src/external_addr.rs` (delete most of file, keep candidate filter)

**Step 1: Delete functions and tests**

Delete:
- `discover_external_ip()` function and all `DISCOVERY_*` constants
- `make_external_addr()` function
- `needs_external_rewrite()` (only used by `make_external_addr`)
- All proptest tests for deleted functions (`rewrites_*`, `skips_*`, `needs_rewrite_*`, `no_rewrite_*`)

Keep:
- `is_circuit_addr()` (used by `is_valid_external_candidate`)
- `has_public_ip()` (used by `is_valid_external_candidate`)
- `is_valid_external_candidate()` (used in node/mod.rs event loop)
- Tests: `valid_candidate_accepts_public_direct_addr`, `valid_candidate_rejects_circuit_addr`, `valid_candidate_rejects_private_ip`, `valid_candidate_rejects_bare_peer_id`

**Step 2: Run tests**

Run: `cargo test -p cli external_addr 2>&1`
Expected: 4 candidate filter tests pass.

**Step 3: Commit**

```bash
git add cli/src/external_addr.rs
git commit -m "refactor: remove manual external IP discovery and address rewriting"
```

---

### Task 8: Simplify node/mod.rs event loop

**Files:**
- Modify: `cli/src/node/mod.rs`

**Step 1: Remove external IP discovery and NodeConfig.external_address**

Delete:
- `external_address` field from `NodeConfig`
- The entire `let external_ip = match config.external_address { ... }` block (lines 82-93)
- The `NewListenAddr` → `make_external_addr` block (lines 163-169) — replace with just logging

**Step 2: Add bootstrap server address confirmation**

After the state machine event processing loop, add:
```rust
// Bootstrap server: confirm observed addresses from Identify
// (first node in network has nobody to AutoNAT it)
if bootstrap_peer_ids.is_empty() {
    if let Event::PeerIdentified { observed_addr, .. } = &state_event {
        swarm.add_external_address(observed_addr.clone());
    }
}
```

**Step 3: Simplify NewListenAddr handler**

Replace the `make_external_addr` block with just:
```rust
if let SwarmEvent::NewListenAddr { address, .. } = &event {
    tracing::info!("listening on {address}");
}
```

**Step 4: Remove unused imports**

Remove `use std::net::IpAddr` and `use crate::external_addr` from the external addr functions (keep the candidate filter import).

Verify `external_addr` is still imported for `is_valid_external_candidate`.

**Step 5: Update NoBootstrapPeers transition**

The event loop currently has:
```rust
if config.bootstrap_addrs.is_empty() {
    let old_phase = app_state.phase();
    let (new_state, commands) = app_state.transition(Event::NoBootstrapPeers);
    ...
}
```
This stays — it triggers Ready for bootstrap servers.

**Step 6: Update Phase references in the event loop**

Replace all `Phase::Participating` with `Phase::Ready`, `Phase::Initializing` with `Phase::Joining`, `Phase::Discovering` with `Phase::Joining`.

**Step 7: Run tests**

Run: `cargo test -p cli 2>&1 | tail -20`
Expected: All tests pass.

**Step 8: Commit**

```bash
git add cli/src/node/mod.rs
git commit -m "refactor(cli): remove manual address management, add bootstrap Identify confirmation"
```

---

### Task 9: Remove --external-address CLI flag and reqwest dependency

**Files:**
- Modify: `cli/src/bin/cli.rs:45-48` (remove flag)
- Modify: `cli/src/bin/cli.rs:60-68` (remove from NodeConfig)
- Modify: `cli/Cargo.toml` (remove reqwest)
- Modify: `Cargo.toml` (workspace, remove reqwest)

**Step 1: Remove CLI flag**

In `cli/src/bin/cli.rs`, delete:
```rust
/// Override external IP address (skip automatic discovery).
#[arg(long, env = "PUNCHGATE_EXTERNAL_ADDRESS")]
external_address: Option<IpAddr>,
```

Remove `use std::net::IpAddr` if no longer needed.

Remove `external_address: cli.external_address` from NodeConfig construction.

**Step 2: Remove reqwest from cli/Cargo.toml**

Delete the `reqwest.workspace = true` line.

**Step 3: Remove reqwest from workspace Cargo.toml**

Delete `reqwest = "0.13.2"` line.

**Step 4: Verify clean build**

Run: `cargo build -p cli 2>&1 | tail -10`
Expected: Clean build, no reqwest.

**Step 5: Commit**

```bash
git add cli/src/bin/cli.rs cli/Cargo.toml Cargo.toml
git commit -m "refactor(cli): remove --external-address flag and reqwest dependency"
```

---

### Task 10: Update E2E compose.yaml and test script

**Files:**
- Modify: `compose.yaml:76-78` (bootstrap env)
- Modify: `compose.yaml:95-100` (workhorse env)
- Modify: `compose.yaml:117-122` (client env)
- Modify: `scripts/e2e_compose_test.sh` (if relay log pattern changes)

**Step 1: Remove PUNCHGATE_EXTERNAL_ADDRESS from compose.yaml**

For bootstrap:
```yaml
environment:
  PUNCHGATE_LISTEN: "/ip4/0.0.0.0/udp/4001/quic-v1"
```

For workhorse:
```yaml
environment:
  PUNCHGATE_LISTEN: "/ip4/0.0.0.0/udp/4001/quic-v1"
  PUNCHGATE_BOOTSTRAP: "/ip4/10.100.0.10/udp/4001/quic-v1/p2p/${BOOTSTRAP_ID}"
  PUNCHGATE_EXPOSE: "echo=10.0.1.20:7777"
  PUNCHGATE_GATEWAY: "10.0.1.2"
  RUST_LOG: "cli=info"
```

For client:
```yaml
environment:
  PUNCHGATE_LISTEN: "/ip4/0.0.0.0/udp/4001/quic-v1"
  PUNCHGATE_BOOTSTRAP: "/ip4/10.100.0.10/udp/4001/quic-v1/p2p/${BOOTSTRAP_ID}"
  PUNCHGATE_TUNNEL_BY_NAME: "echo@0.0.0.0:2222"
  PUNCHGATE_GATEWAY: "10.0.2.2"
  RUST_LOG: "cli=info"
```

**Step 2: Update e2e_compose_test.sh log patterns**

The test waits for "relay reservation accepted" — this pattern depends on AutoNAT confirming Private status now. The e2e NAT topology uses full-cone NAT which should trigger AutoNAT Private.

If the relay log message changes, update the `wait_for_container_log` pattern. Keep the existing pattern if it still works.

Also update the tunnel test script comment that references `--nat-status private` (line 25-26) — remove `--nat-status` reference since it no longer exists.

**Step 3: Commit**

```bash
git add compose.yaml scripts/e2e_compose_test.sh scripts/e2e_tunnel_test.sh
git commit -m "refactor(e2e): remove PUNCHGATE_EXTERNAL_ADDRESS from compose config"
```

---

### Task 11: Run full test suite and E2E

**Step 1: Run unit + integration tests**

Run: `cargo test -p cli 2>&1 | tail -30`
Expected: All tests pass (property tests + integration tests).

**Step 2: Run E2E tunnel test**

Run: `scripts/e2e_tunnel_test.sh`
Expected: PASS — direct LAN scenario doesn't need external addresses.

**Step 3: Run E2E compose test (NAT topology)**

Run: `scripts/e2e_compose_test.sh`
Expected: PASS — bootstrap confirms address via Identify, NATted peers get addresses from AutoNAT.

**Step 4: If E2E compose test fails**

Debug by checking:
1. Does bootstrap get `ExternalAddrConfirmed` from Identify? Check bootstrap logs.
2. Does workhorse get `NatStatus::Private` from AutoNAT? Check workhorse logs.
3. Does relay reservation succeed? Check for "relay reservation accepted".
4. Adjust compose setup if AutoNAT needs more peers to probe.

**Step 5: Commit any fixes**

---

### Task 12: Clean up and final commit

**Step 1: Remove any dead code**

Run: `cargo clippy -p cli 2>&1 | grep "unused"`
Fix any dead imports, unused functions, etc.

**Step 2: Final test run**

Run: `cargo test -p cli 2>&1 | tail -20`
Expected: All green.

**Step 3: Commit cleanup**

```bash
git commit -m "chore: clean up dead code after native pipeline simplification"
```
