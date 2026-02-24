# NAT Traversal Reliability Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Fix four NAT traversal issues — filter unreachable addresses from Kademlia, increase relay circuit duration, add periodic hole-punch retry, and set `holepunch_failed` on timeout.

**Architecture:** Four independent fixes in the coalgebraic state machine. Fixes 1/2/4 are surgical edits. Fix 3 adds a new event (`HolePunchRetryTick`), a new command (`RetryDirectDial`), state machine handling in `TunnelState`, and a timer in the event loop — following the same pattern as existing `holepunch_deadlines`.

**Tech Stack:** Rust, libp2p 0.56, tokio, proptest

---

### Task 1: Fix 4 — Set `holepunch_failed` on timeout (simplest, unblocks understanding)

**Files:**
- Modify: `cli/src/state/tunnel.rs:240-254`
- Modify: `cli/src/state/tunnel.rs:772-789` (existing test)

**Step 1: Update the existing test to assert `holepunch_failed` is set**

In `cli/src/state/tunnel.rs`, find the test `holepunch_timeout_spawns_on_relay` (line 772). Add an assertion that `state.holepunch_failed` contains the peer after timeout:

```rust
        #[test]
        fn holepunch_timeout_spawns_on_relay(
            peer in arb_peer_id(),
            service in arb_service_name(),
            bind in arb_service_addr(),
        ) {
            let mut state = TunnelState::new();
            state.relayed_peers.insert(peer);
            state.awaiting_holepunch.insert(peer, vec![(service.clone(), bind.clone())]);

            let (state, commands) = state.transition(Event::HolePunchTimeout { peer });

            let has_spawn = commands.contains(&Command::SpawnTunnel {
                peer, service, bind, relayed: true,
            });
            prop_assert!(has_spawn);
            prop_assert!(!state.awaiting_holepunch.contains_key(&peer));
            prop_assert!(state.holepunch_failed.contains(&peer));
        }
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p cli --lib -- tunnel::tests::holepunch_timeout_spawns_on_relay`
Expected: FAIL — `holepunch_failed` does not contain peer

**Step 3: Add `holepunch_failed.insert(peer)` to the timeout handler**

In `cli/src/state/tunnel.rs`, modify the `HolePunchTimeout` handler (line 240) to insert into `holepunch_failed` before spawning:

```rust
            Event::HolePunchTimeout { peer } => {
                self.holepunch_failed.insert(peer);
                if let Some(specs) = self.awaiting_holepunch.remove(&peer) {
                    tracing::info!(
                        peer = %peer,
                        "hole punch timeout, falling back to relay for tunnel"
                    );
                    commands.extend(specs.into_iter().map(|(service, bind)| {
                        Command::SpawnTunnel {
                            peer,
                            service,
                            bind,
                            relayed: true,
                        }
                    }));
                }
            }
```

**Step 4: Run test to verify it passes**

Run: `cargo test -p cli --lib -- tunnel::tests::holepunch_timeout_spawns_on_relay`
Expected: PASS

**Step 5: Run full test suite**

Run: `cargo test -p cli --lib`
Expected: All tests pass

**Step 6: Commit**

```bash
git add cli/src/state/tunnel.rs
git commit -m "fix(state): set holepunch_failed on timeout for consistency"
```

---

### Task 2: Fix 2 — Increase relay `max_circuit_duration`

**Files:**
- Modify: `cli/src/protocol.rs:13` (add constant)
- Modify: `cli/src/behaviour.rs:38` (use custom relay config)

**Step 1: Add `MAX_CIRCUIT_DURATION` constant to protocol.rs**

In `cli/src/protocol.rs`, after line 13 (`HOLEPUNCH_TIMEOUT`), add:

```rust
pub const MAX_CIRCUIT_DURATION: Duration = Duration::from_secs(3600);
```

**Step 2: Use custom relay::Config in Behaviour::new**

In `cli/src/behaviour.rs`, replace line 38:

```rust
        let relay_server = relay::Behaviour::new(peer_id, Default::default());
```

with:

```rust
        let relay_config = relay::Config {
            max_circuit_duration: protocol::MAX_CIRCUIT_DURATION,
            ..Default::default()
        };
        let relay_server = relay::Behaviour::new(peer_id, relay_config);
```

**Step 3: Verify it compiles and tests pass**

Run: `cargo test -p cli --lib`
Expected: All tests pass (no relay-specific unit tests; this is a config change)

**Step 4: Commit**

```bash
git add cli/src/protocol.rs cli/src/behaviour.rs
git commit -m "fix(relay): increase max_circuit_duration to 3600s to prevent tunnel drops"
```

---

### Task 3: Fix 1 — Filter private/loopback addresses from Kademlia

**Files:**
- Modify: `cli/src/state/peer.rs:192-206` (PeerIdentified handler)
- Modify: `cli/src/state/peer.rs:601-624` (update existing test)
- Add test in: `cli/src/state/peer.rs` (new test for filtering)

**Step 1: Write a test that PeerIdentified only pushes public addrs to Kademlia**

In `cli/src/state/peer.rs`, inside the `proptest!` block in `mod tests`, add a new test after `peer_identified_adds_all_addresses`:

```rust
        #[test]
        fn peer_identified_filters_private_addrs_from_kademlia(
            peer in arb_peer_id(),
            public_addr in arb_public_multiaddr(),
            private_addr in arb_private_multiaddr(),
            observed in arb_multiaddr(),
        ) {
            let state = PeerState::new();
            let (new_state, commands) = state.transition(Event::PeerIdentified {
                peer,
                listen_addrs: vec![public_addr.clone(), private_addr.clone()],
                observed_addr: observed,
            });

            // Both addresses stored in known_peers
            let stored = new_state.known_peers.get(&peer)
                .expect("peer should be in known_peers after PeerIdentified");
            prop_assert!(stored.contains(&public_addr));
            prop_assert!(stored.contains(&private_addr));

            // Only public address pushed to Kademlia
            let kad_addrs: Vec<_> = commands.iter()
                .filter_map(|c| match c {
                    Command::KademliaAddAddress { addr, .. } => Some(addr),
                    _ => None,
                })
                .collect();
            prop_assert_eq!(kad_addrs.len(), 1);
            prop_assert_eq!(kad_addrs[0], &public_addr);
        }
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p cli --lib -- peer::tests::peer_identified_filters_private_addrs_from_kademlia`
Expected: FAIL — both addresses are pushed to Kademlia

**Step 3: Filter addresses in PeerIdentified handler**

In `cli/src/state/peer.rs`, modify the `PeerIdentified` handler (lines 192-206):

```rust
            Event::PeerIdentified {
                peer, listen_addrs, ..
            } => {
                let entry = self.known_peers.entry(peer).or_default();
                for addr in &listen_addrs {
                    entry.insert(addr.clone());
                    if crate::external_addr::has_public_ip(addr) {
                        commands.push(Command::KademliaAddAddress {
                            peer,
                            addr: addr.clone(),
                        });
                    }
                }
                if self.bootstrap_peers.contains(&peer) {
                    commands.extend(self.maybe_request_relay());
                }
            }
```

**Step 4: Update the existing `peer_identified_adds_all_addresses` test**

The existing test at line 601 asserts `kad_add_count == addrs.len()`. With filtering, this is no longer true since `arb_multiaddr()` generates random IPs that may or may not be public. Update the test to use public addresses explicitly:

```rust
        #[test]
        fn peer_identified_adds_all_addresses(
            peer in arb_peer_id(),
            addrs in proptest::collection::vec(arb_public_multiaddr(), 1..5),
            observed in arb_multiaddr(),
        ) {
            let state = PeerState::new();
            let (new_state, commands) = state.transition(Event::PeerIdentified {
                peer,
                listen_addrs: addrs.clone(),
                observed_addr: observed,
            });

            let stored = new_state.known_peers.get(&peer)
                .expect("peer should be in known_peers after PeerIdentified");
            for addr in &addrs {
                prop_assert!(stored.contains(addr));
            }

            let kad_add_count = commands.iter()
                .filter(|c| matches!(c, Command::KademliaAddAddress { .. }))
                .count();
            prop_assert_eq!(kad_add_count, addrs.len());
        }
```

**Step 5: Run tests to verify they pass**

Run: `cargo test -p cli --lib -- peer::tests`
Expected: All peer tests pass

**Step 6: Run full test suite**

Run: `cargo test -p cli --lib`
Expected: All tests pass

**Step 7: Commit**

```bash
git add cli/src/state/peer.rs
git commit -m "fix(state): filter private/loopback addresses from Kademlia in PeerIdentified"
```

---

### Task 4: Fix 3 — Periodic hole-punch retry via direct dial

This is the most complex fix. It adds a new event, a new command, state machine handling, and a timer in the event loop.

#### Step 1: Add protocol constant

**File:** `cli/src/protocol.rs`

After `HOLEPUNCH_TIMEOUT`, add:

```rust
pub const HOLEPUNCH_RETRY_INTERVAL: Duration = Duration::from_secs(60);
```

#### Step 2: Add `HolePunchRetryTick` event variant

**File:** `cli/src/state/event.rs`

After `HolePunchTimeout` (line 78-80), add:

```rust
    HolePunchRetryTick {
        peer: PeerId,
    },
```

#### Step 3: Add `RetryDirectDial` command variant

**File:** `cli/src/state/command.rs`

After `AwaitHolePunch` (line 43-45), add:

```rust
    RetryDirectDial {
        peer: PeerId,
    },
```

#### Step 4: Write failing tests for the retry logic

**File:** `cli/src/state/tunnel.rs` — add inside `proptest!` block, after `holepunch_timeout_spawns_on_relay`:

```rust
        #[test]
        fn holepunch_retry_dials_relayed_peer(
            peer in arb_peer_id(),
        ) {
            let mut state = TunnelState::new();
            state.relayed_peers.insert(peer);

            let (_, commands) = state.transition(Event::HolePunchRetryTick { peer });

            let has_dial = commands.contains(&Command::RetryDirectDial { peer });
            prop_assert!(has_dial);
        }

        #[test]
        fn holepunch_retry_noop_for_non_relayed_peer(
            peer in arb_peer_id(),
        ) {
            let state = TunnelState::new();

            let (_, commands) = state.transition(Event::HolePunchRetryTick { peer });

            prop_assert!(commands.is_empty());
        }

        #[test]
        fn direct_connection_clears_relayed_after_retry(
            peer in arb_peer_id(),
        ) {
            let mut state = TunnelState::new();
            state.relayed_peers.insert(peer);
            state.holepunch_failed.insert(peer);

            let (state, _) = state.transition(Event::TunnelPeerConnected {
                peer, relayed: false,
            });

            prop_assert!(!state.relayed_peers.contains(&peer));
            prop_assert!(!state.holepunch_failed.contains(&peer));
        }
```

#### Step 5: Run tests to verify they fail

Run: `cargo test -p cli --lib -- tunnel::tests::holepunch_retry`
Expected: FAIL — compilation error (new event variant not handled)

#### Step 6: Handle `HolePunchRetryTick` in TunnelState

**File:** `cli/src/state/tunnel.rs`

In the `transition` method, add a new match arm before the catch-all `Event::PhaseChanged { .. } | ...` arm (before line 274):

```rust
            Event::HolePunchRetryTick { peer } => {
                if self.relayed_peers.contains(&peer) {
                    commands.push(Command::RetryDirectDial { peer });
                }
            }
```

Also add `Event::HolePunchRetryTick { .. }` to the PeerState catch-all in `cli/src/state/peer.rs` (line 217, inside the `HolePunchSucceeded | HolePunchFailed | ...` arm):

```rust
            Event::HolePunchSucceeded { .. }
            | Event::HolePunchFailed { .. }
            | Event::HolePunchTimeout { .. }
            | Event::HolePunchRetryTick { .. }
            | Event::DhtPeerLookupComplete { .. }
            ...
```

#### Step 7: Run tests to verify they pass

Run: `cargo test -p cli --lib -- tunnel::tests`
Expected: All tunnel tests pass

#### Step 8: Execute `RetryDirectDial` in the command executor

**File:** `cli/src/node/execute.rs`

Add a new match arm in `execute_commands` after `AwaitHolePunch` (after line 119):

```rust
            Command::RetryDirectDial { peer } => {
                if swarm.is_connected(peer) {
                    tracing::debug!(%peer, "skipping retry dial — already connected");
                } else {
                    tracing::info!(%peer, "retrying direct dial for hole-punch");
                    match swarm.dial(*peer) {
                        Ok(()) => {}
                        Err(e) => tracing::debug!(%peer, error = %e, "retry dial failed"),
                    }
                }
            }
```

#### Step 9: Add retry timer to the event loop

**File:** `cli/src/node/mod.rs`

**9a.** Add import for `HOLEPUNCH_RETRY_INTERVAL` — the existing import at line 24 already imports from `protocol`. Add the new constant to the import:

In line 24, change:
```rust
    protocol::{self, DISCOVERY_TIMEOUT, HOLEPUNCH_TIMEOUT, IDLE_CONNECTION_TIMEOUT},
```
to:
```rust
    protocol::{self, DISCOVERY_TIMEOUT, HOLEPUNCH_RETRY_INTERVAL, HOLEPUNCH_TIMEOUT, IDLE_CONNECTION_TIMEOUT},
```

**9b.** Add `holepunch_retries` map alongside `holepunch_deadlines` (after line 148):

```rust
    let mut holepunch_retries: HashMap<PeerId, tokio::time::Instant> = HashMap::new();
```

**9c.** In the `loop` block, compute `next_holepunch_retry` alongside the other next-timers (after line 155):

```rust
        let next_holepunch_retry = holepunch_retries.values().min().copied();
```

**9d.** In the event handler, clear retries on direct connection or disconnect. Inside the `match &state_event` block (around lines 231-249), add arms:

After the existing `Event::TunnelPeerConnected { peer, relayed: false }` arm (line 243-244), and after `Event::ConnectionLost` arm (line 246-248), extend them:

```rust
                        Event::TunnelPeerConnected { peer, relayed: false } => {
                            holepunch_deadlines.remove(peer);
                            holepunch_retries.remove(peer);
                        }
                        Event::ConnectionLost { peer, remaining_connections: 0 } => {
                            holepunch_deadlines.remove(peer);
                            holepunch_retries.remove(peer);
                        }
                        Event::HolePunchSucceeded { remote_peer } => {
                            tracing::info!(peer = %remote_peer, "hole punch succeeded");
                            holepunch_deadlines.remove(remote_peer);
                            holepunch_retries.remove(remote_peer);
                        }
```

**9e.** After the holepunch_deadline timeout fires and spawns the relay tunnel (inside the for loop at line 337-346), schedule a retry:

```rust
                for peer in due {
                    holepunch_deadlines.remove(&peer);
                    tracing::info!(%peer, "hole punch timeout, falling back to relay");
                    let old_phase = app_state.phase();
                    let (new_state, commands) = app_state.transition(Event::HolePunchTimeout { peer });
                    tracing::debug!(event = "HolePunchTimeout", commands = commands.len(), "event processed");
                    log_phase_transition(old_phase, new_state.phase(), commands.len());
                    app_state = new_state;
                    execute_commands(&mut swarm, &commands, &config.exposed, &mut ctx);

                    // Schedule periodic hole-punch retry
                    holepunch_retries.entry(peer)
                        .or_insert(tokio::time::Instant::now() + HOLEPUNCH_RETRY_INTERVAL);
                }
```

**9f.** Add a new `tokio::select!` arm for the retry timer. Place it after the holepunch deadline arm (after line 347) and before the shutdown arm:

```rust
            _ = async {
                match next_holepunch_retry {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending::<()>().await,
                }
            } => {
                let now = tokio::time::Instant::now();
                let due: Vec<PeerId> = holepunch_retries
                    .iter()
                    .filter(|(_, deadline)| **deadline <= now)
                    .map(|(pid, _)| *pid)
                    .collect();

                for peer in due {
                    holepunch_retries.remove(&peer);
                    tracing::info!(%peer, "retrying hole-punch via direct dial");
                    let old_phase = app_state.phase();
                    let (new_state, commands) = app_state.transition(Event::HolePunchRetryTick { peer });
                    tracing::debug!(event = "HolePunchRetryTick", commands = commands.len(), "event processed");
                    log_phase_transition(old_phase, new_state.phase(), commands.len());
                    app_state = new_state;
                    execute_commands(&mut swarm, &commands, &config.exposed, &mut ctx);

                    // Reschedule if peer is still relayed (command was emitted)
                    if commands.iter().any(|c| matches!(c, Command::RetryDirectDial { .. })) {
                        holepunch_retries.insert(peer, tokio::time::Instant::now() + HOLEPUNCH_RETRY_INTERVAL);
                    }
                }
            }
```

#### Step 10: Verify compilation and all tests pass

Run: `cargo test -p cli --lib`
Expected: All tests pass

Run: `cargo test -p cli --test integration`
Expected: Integration tests pass

#### Step 11: Commit

```bash
git add cli/src/protocol.rs cli/src/state/event.rs cli/src/state/command.rs cli/src/state/tunnel.rs cli/src/state/peer.rs cli/src/node/mod.rs cli/src/node/execute.rs
git commit -m "feat(nat): add periodic hole-punch retry via direct dial after relay fallback"
```

---

### Task 5: Final verification

**Step 1: Run full test suite**

Run: `cargo test -p cli`
Expected: All lib + integration tests pass

**Step 2: Build release binary**

Run: `cargo build --release`
Expected: Compiles without warnings

**Step 3: Commit any remaining changes**

If there are compiler warnings to fix, address them and commit.
