# DCUtR Hole-Punch Proof-of-Concept Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Fix the tunnel state machine deadlock and prove DCUtR hole-punching works end-to-end in the container E2E topology with full-cone NAT.

**Architecture:** Add `HolePunchTimeout` event and `AwaitHolePunch` command to the coalgebraic state machine. The tunnel state machine waits for DCUtR result with a 15s timeout, spawning on direct connection when hole-punch succeeds, falling back to relay on failure or timeout. The event loop tracks per-peer deadlines and synthesizes timeout events.

**Tech Stack:** Rust, libp2p 0.56, tokio, proptest, podman compose

---

### Task 1: Add protocol constant and new types

**Files:**
- Modify: `cli/src/protocol.rs:12` (add constant after SERVICE_REPUBLISH_INTERVAL)
- Modify: `cli/src/state/event.rs:77` (add variant after HolePunchFailed)
- Modify: `cli/src/state/command.rs:37-41` (add relayed field to SpawnTunnel, add AwaitHolePunch)

**Step 1: Add HOLEPUNCH_TIMEOUT constant**

In `cli/src/protocol.rs`, add after line 12:

```rust
pub const HOLEPUNCH_TIMEOUT: Duration = Duration::from_secs(15);
```

**Step 2: Add HolePunchTimeout event variant**

In `cli/src/state/event.rs`, add after `HolePunchFailed` (after line 77):

```rust
    HolePunchTimeout {
        peer: PeerId,
    },
```

**Step 3: Add `relayed` field to SpawnTunnel and add AwaitHolePunch command**

In `cli/src/state/command.rs`, replace lines 37-41:

```rust
    SpawnTunnel {
        peer: PeerId,
        service: ServiceName,
        bind: ServiceAddr,
        relayed: bool,
    },
    AwaitHolePunch {
        peer: PeerId,
    },
```

**Step 4: Verify it compiles (expect errors from consumers)**

Run: `cargo check -p cli 2>&1 | head -40`

Expected: Compilation errors in `tunnel.rs`, `execute.rs`, `integration.rs` where `SpawnTunnel` is constructed without `relayed` field, and missing `HolePunchTimeout` / `AwaitHolePunch` match arms. This is expected — we fix those in subsequent tasks.

---

### Task 2: Update tunnel state machine — SpawnTunnel relayed field

**Files:**
- Modify: `cli/src/state/tunnel.rs:73-103` (route_connected_peer)
- Modify: `cli/src/state/tunnel.rs:169-195` (TunnelPeerConnected)
- Modify: `cli/src/state/tunnel.rs:197-211` (HolePunchSucceeded)

This task adds the `relayed` field to all `SpawnTunnel` constructions and the `AwaitHolePunch` command emission, WITHOUT changing behavior yet. `HolePunchFailed` and `HolePunchTimeout` are handled in Task 3.

**Step 1: Update `route_connected_peer`**

In `cli/src/state/tunnel.rs`, replace `route_connected_peer` (lines 73-103) with:

```rust
    fn route_connected_peer(
        &mut self,
        peer: PeerId,
        specs: Vec<(ServiceName, ServiceAddr)>,
    ) -> Vec<Command> {
        if self.holepunch_failed.contains(&peer) {
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
            specs
                .into_iter()
                .map(|(service, bind)| Command::SpawnTunnel {
                    peer,
                    service,
                    bind,
                    relayed: false,
                })
                .collect()
        }
    }
```

**Step 2: Update `TunnelPeerConnected { relayed: true }` to emit AwaitHolePunch**

In the `TunnelPeerConnected` handler (lines 169-195), replace the `true` arm (lines 172-176):

```rust
                    true => {
                        self.relayed_peers.insert(peer);
                        if let Some(specs) = self.pending_by_peer.remove(&peer) {
                            self.awaiting_holepunch.insert(peer, specs);
                            commands.push(Command::AwaitHolePunch { peer });
                        }
                    }
```

**Step 3: Update `TunnelPeerConnected { relayed: false }` SpawnTunnel construction**

Replace the `false` arm (lines 178-193):

```rust
                    false => {
                        self.relayed_peers.remove(&peer);
                        self.holepunch_failed.remove(&peer);
                        commands.extend(
                            self.pending_by_peer
                                .remove(&peer)
                                .into_iter()
                                .chain(self.awaiting_holepunch.remove(&peer))
                                .flatten()
                                .map(|(service, bind)| Command::SpawnTunnel {
                                    peer,
                                    service,
                                    bind,
                                    relayed: false,
                                }),
                        );
                    }
```

**Step 4: Update `HolePunchSucceeded` SpawnTunnel construction**

Replace `HolePunchSucceeded` handler (lines 197-211):

```rust
            Event::HolePunchSucceeded { remote_peer } => {
                self.relayed_peers.remove(&remote_peer);
                self.holepunch_failed.remove(&remote_peer);
                commands.extend(
                    self.awaiting_holepunch
                        .remove(&remote_peer)
                        .into_iter()
                        .flatten()
                        .map(|(service, bind)| Command::SpawnTunnel {
                            peer: remote_peer,
                            service,
                            bind,
                            relayed: false,
                        }),
                );
            }
```

---

### Task 3: Update tunnel state machine — HolePunchFailed fallback and HolePunchTimeout

**Files:**
- Modify: `cli/src/state/tunnel.rs:213-228` (HolePunchFailed handler)
- Modify: `cli/src/state/tunnel.rs:247-262` (wildcard arm — add HolePunchTimeout)

**Step 1: Change `HolePunchFailed` to spawn on relay instead of dropping**

Replace lines 213-228:

```rust
            Event::HolePunchFailed {
                remote_peer,
                ref reason,
            } => {
                self.holepunch_failed.insert(remote_peer);
                if let Some(specs) = self.awaiting_holepunch.remove(&remote_peer) {
                    tracing::info!(
                        peer = %remote_peer,
                        reason = %reason,
                        "hole punch failed, falling back to relay for tunnel"
                    );
                    commands.extend(specs.into_iter().map(|(service, bind)| {
                        Command::SpawnTunnel {
                            peer: remote_peer,
                            service,
                            bind,
                            relayed: true,
                        }
                    }));
                }
            }
```

**Step 2: Add `HolePunchTimeout` handler**

Add BEFORE the wildcard arm (before line 247):

```rust
            Event::HolePunchTimeout { peer } => {
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

**Step 3: Add `HolePunchTimeout` to the wildcard noop arm**

The wildcard arm currently does NOT list `HolePunchTimeout`. Since we handled it above, it won't reach the wildcard. But verify the exhaustive match still compiles. No change needed if the new arm is above the wildcard.

---

### Task 4: Update execute.rs — handle new commands

**Files:**
- Modify: `cli/src/node/execute.rs:96-113` (SpawnTunnel handler)
- Modify: `cli/src/node/execute.rs` (add AwaitHolePunch — no-op in executor, handled by event loop)

**Step 1: Add `relayed` to SpawnTunnel destructure and log**

Replace lines 96-113:

```rust
            Command::SpawnTunnel {
                peer,
                service,
                bind,
                relayed,
            } => {
                let control = ctx.stream_control.clone();
                let peer = *peer;
                let svc = service.clone();
                let addr = bind.clone();
                let relayed = *relayed;
                let label = format!("{peer}:{svc}@{addr}");
                tracing::info!(%peer, service = %svc, bind = %addr, relayed, "spawning tunnel");
                let handle = tokio::spawn(async move {
                    match tunnel::connect_tunnel(control, peer, svc, addr).await {
                        Ok(()) => {}
                        Err(e) => tracing::error!(error = %e, "tunnel failed"),
                    }
                });
                ctx.tunnel_registry.register(label, handle);
            }
```

**Step 2: Add AwaitHolePunch handler (no-op in executor)**

Add after the SpawnTunnel handler, before the closing `}` of the match:

```rust
            Command::AwaitHolePunch { .. } => {
                // Handled by the event loop's holepunch deadline tracking, not the executor
            }
```

**Step 3: Verify compilation**

Run: `cargo check -p cli --lib`

Expected: Compiles cleanly (integration tests may still fail — we fix those in Task 6).

---

### Task 5: Update event loop — holepunch deadline tracking

**Files:**
- Modify: `cli/src/node/mod.rs:24` (add HOLEPUNCH_TIMEOUT import)
- Modify: `cli/src/node/mod.rs:138` (add holepunch_deadlines HashMap)
- Modify: `cli/src/node/mod.rs:211-248` (clear deadlines on relevant events)
- Modify: `cli/src/node/mod.rs:239` (intercept AwaitHolePunch command to set deadline)
- Modify: `cli/src/node/mod.rs:267-298` (add holepunch timeout select! branch)

**Step 1: Add import**

In line 24, change:

```rust
    protocol::{self, DISCOVERY_TIMEOUT, IDLE_CONNECTION_TIMEOUT},
```

to:

```rust
    protocol::{self, DISCOVERY_TIMEOUT, HOLEPUNCH_TIMEOUT, IDLE_CONNECTION_TIMEOUT},
```

**Step 2: Add holepunch_deadlines state**

After line 138 (`let mut pending_reconnects: HashMap<PeerId, tokio::time::Instant> = HashMap::new();`), add:

```rust
    let mut holepunch_deadlines: HashMap<PeerId, tokio::time::Instant> = HashMap::new();
```

**Step 3: Clear deadlines when processing events**

Inside the event processing loop (inside `for state_event in events {`), after the existing match block (after line 232), add:

```rust
                    match &state_event {
                        Event::HolePunchSucceeded { remote_peer } => {
                            holepunch_deadlines.remove(remote_peer);
                        }
                        Event::HolePunchFailed { remote_peer, .. } => {
                            holepunch_deadlines.remove(remote_peer);
                        }
                        Event::TunnelPeerConnected { peer, relayed: false } => {
                            holepunch_deadlines.remove(peer);
                        }
                        Event::ConnectionLost { peer, remaining_connections: 0 } => {
                            holepunch_deadlines.remove(peer);
                        }
                        _ => {}
                    }
```

**Step 4: Intercept AwaitHolePunch commands after execute_commands**

After `execute_commands(&mut swarm, &commands, &config.exposed, &mut ctx);` (line 239), add:

```rust
                    for cmd in &commands {
                        if let Command::AwaitHolePunch { peer } = cmd {
                            holepunch_deadlines.entry(*peer)
                                .or_insert(tokio::time::Instant::now() + HOLEPUNCH_TIMEOUT);
                        }
                    }
```

**Step 5: Add holepunch timeout select! branch**

Add a new `select!` branch after the reconnect timer branch (after line 298, before the shutdown signal branch). Also update the `next_reconnect` computation at line 144 to compute `next_holepunch`:

First, add at line 144 (alongside `next_reconnect`):

```rust
        let next_holepunch = holepunch_deadlines.values().min().copied();
```

Then add the new select! branch:

```rust
            _ = async {
                match next_holepunch {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending::<()>().await,
                }
            } => {
                let now = tokio::time::Instant::now();
                let due: Vec<PeerId> = holepunch_deadlines
                    .iter()
                    .filter(|(_, deadline)| **deadline <= now)
                    .map(|(pid, _)| *pid)
                    .collect();

                for peer in due {
                    holepunch_deadlines.remove(&peer);
                    tracing::info!(%peer, "hole punch timeout, falling back to relay");
                    let old_phase = app_state.phase();
                    let (new_state, commands) = app_state.transition(Event::HolePunchTimeout { peer });
                    tracing::debug!(event = "HolePunchTimeout", commands = commands.len(), "event processed");
                    log_phase_transition(old_phase, new_state.phase(), commands.len());
                    app_state = new_state;
                    execute_commands(&mut swarm, &commands, &config.exposed, &mut ctx);
                }
            }
```

**Step 6: Add Command import for the AwaitHolePunch interception**

In line 27, the `state` import already includes `Command` implicitly through `AppState`. Verify `Command` is accessible. If not, add it to the use statement at line 27:

```rust
    state::{AppState, Command, Event, PeerState, Phase, TunnelState},
```

**Step 7: Verify compilation**

Run: `cargo check -p cli --lib`

Expected: Compiles cleanly.

---

### Task 6: Update tests — new SpawnTunnel field

**Files:**
- Modify: `cli/src/state/tunnel.rs` (tests section, lines 271-735)

All test assertions that check for `Command::SpawnTunnel` need the new `relayed` field. Tests that check relayed routing need to be updated for the new `AwaitHolePunch` command.

**Step 1: Update `dht_peer_lookup_spawns_when_connected`**

Change (line 355-357):
```rust
            let has_spawn = commands.contains(&Command::SpawnTunnel {
                peer, service, bind,
            });
```
to:
```rust
            let has_spawn = commands.contains(&Command::SpawnTunnel {
                peer, service, bind, relayed: false,
            });
```

**Step 2: Update `dht_service_resolved_spawns_when_connected`**

Change (line 396-398):
```rust
            let has_spawn = commands.contains(&Command::SpawnTunnel {
                peer: provider, service, bind,
            });
```
to:
```rust
            let has_spawn = commands.contains(&Command::SpawnTunnel {
                peer: provider, service, bind, relayed: false,
            });
```

**Step 3: Update `tunnel_peer_connected_direct_spawns_immediately`**

Change (line 435-437):
```rust
            let has_spawn = commands.contains(&Command::SpawnTunnel {
                peer, service, bind,
            });
```
to:
```rust
            let has_spawn = commands.contains(&Command::SpawnTunnel {
                peer, service, bind, relayed: false,
            });
```

**Step 4: Update `tunnel_peer_connected_relayed_waits_for_holepunch`**

The test asserts `commands.is_empty()` (line 454). Now it should emit `AwaitHolePunch`. Change:

```rust
            prop_assert!(commands.is_empty());
```
to:
```rust
            let has_await = commands.contains(&Command::AwaitHolePunch { peer });
            prop_assert!(has_await);
```

**Step 5: Update `hole_punch_succeeded_spawns_waiting_tunnels`**

Change (line 471-473):
```rust
            let has_spawn = commands.contains(&Command::SpawnTunnel {
                peer, service, bind,
            });
```
to:
```rust
            let has_spawn = commands.contains(&Command::SpawnTunnel {
                peer, service, bind, relayed: false,
            });
```

**Step 6: Update `hole_punch_failed_removes_pending` — now spawns on relay**

This test's behavior changes. Replace the whole test body assertions (lines 488-493):

```rust
            let (state, commands) = state.transition(Event::HolePunchFailed {
                remote_peer: peer, reason,
            });

            let has_spawn = commands.contains(&Command::SpawnTunnel {
                peer, service, bind, relayed: true,
            });
            prop_assert!(has_spawn);
            prop_assert!(!state.awaiting_holepunch.contains_key(&peer));
```

Also rename the test to `hole_punch_failed_spawns_on_relay`.

**Step 7: Update `dht_peer_lookup_relayed_awaits_holepunch`**

Change assertion from `commands.is_empty()` to check for AwaitHolePunch:

```rust
            let has_await = commands.contains(&Command::AwaitHolePunch { peer });
            prop_assert!(has_await);
```

**Step 8: Update `dht_service_resolved_relayed_awaits_holepunch`**

Same change:

```rust
            let has_await = commands.contains(&Command::AwaitHolePunch { peer: provider });
            prop_assert!(has_await);
```

**Step 9: Update `dht_peer_lookup_holepunch_failed_drops_specs`**

With the new behavior, holepunch_failed routes to SpawnTunnel with relayed: true. Change assertion:

```rust
            let has_spawn = commands.contains(&Command::SpawnTunnel {
                peer, service, bind, relayed: true,
            });
            prop_assert!(has_spawn);
            prop_assert!(!state.pending_by_peer.contains_key(&peer));
```

**Step 10: Update `dht_service_resolved_holepunch_failed_drops_specs`**

Same change:

```rust
            let has_spawn = commands.contains(&Command::SpawnTunnel {
                peer: provider, service, bind, relayed: true,
            });
            prop_assert!(has_spawn);
```

**Step 11: Update `direct_connection_spawns_from_awaiting_holepunch`**

Change (line 645-647):
```rust
            let has_spawn = commands.contains(&Command::SpawnTunnel {
                peer, service, bind,
            });
```
to:
```rust
            let has_spawn = commands.contains(&Command::SpawnTunnel {
                peer, service, bind, relayed: false,
            });
```

**Step 12: Update `holepunch_failed_then_direct_clears_failure`**

Change (line 699-701):
```rust
            let has_spawn = commands.contains(&Command::SpawnTunnel {
                peer, service, bind,
            });
```
to:
```rust
            let has_spawn = commands.contains(&Command::SpawnTunnel {
                peer, service, bind, relayed: false,
            });
```

**Step 13: Update `holepunch_succeeded_spawns_multiple_waiting_tunnels`**

Change both SpawnTunnel checks (lines 724-731):
```rust
            let has_first = commands.contains(&Command::SpawnTunnel {
                peer, service: s1, bind: b1, relayed: false,
            });
            prop_assert!(has_first);
            let has_second = commands.contains(&Command::SpawnTunnel {
                peer, service: s2, bind: b2, relayed: false,
            });
```

**Step 14: Add new test for HolePunchTimeout**

Add a new test after the existing tests:

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
        }
```

**Step 15: Run all unit tests**

Run: `cargo test -p cli --lib`

Expected: All tests pass.

---

### Task 7: Update integration tests

**Files:**
- Modify: `cli/tests/integration.rs` (grep for `SpawnTunnel` — not present in integration tests since they use real swarms, not state machine assertions)

**Step 1: Check if integration tests reference SpawnTunnel**

Run: `grep -n "SpawnTunnel" cli/tests/integration.rs`

If no matches, no changes needed. Integration tests use real swarms and don't assert on Command types.

**Step 2: Run integration tests**

Run: `cargo test -p cli --test integration`

Expected: All tests pass. The integration tests exercise the real event loop which now has the updated command handling.

---

### Task 8: Update E2E compose test

**Files:**
- Modify: `scripts/e2e_compose_test.sh:205-217` (Step 8: DCUtR assertion)

**Step 1: Make DCUtR assertion a hard pass/fail**

Replace lines 205-217 with:

```bash
# ── Step 8: Verify DCUtR hole punch ──────────────────────────────────

CLEAN_LOGS=$($COMPOSE logs client workhorse 2>&1 | sed 's/\x1b\[[0-9;]*m//g')
HP_SUCCESS=$(echo "$CLEAN_LOGS" | grep -c "hole punch succeeded" || true)
HP_FAILURE=$(echo "$CLEAN_LOGS" | grep -c "hole punch failed" || true)

if [[ "$HP_SUCCESS" -gt 0 ]]; then
    pass "DCUtR hole punch succeeded ($HP_SUCCESS event(s))"
elif [[ "$HP_FAILURE" -gt 0 ]]; then
    warn "DCUtR hole punch failed ($HP_FAILURE event(s))"
    warn "── client log (last 50 lines) ──"
    $COMPOSE logs --tail 50 client 2>&1 || true
    warn "── workhorse log (last 50 lines) ──"
    $COMPOSE logs --tail 50 workhorse 2>&1 || true
    fail "DCUtR hole punch failed — expected success with full-cone NAT"
else
    warn "no DCUtR events detected"
    warn "── client log (last 50 lines) ──"
    $COMPOSE logs --tail 50 client 2>&1 || true
    warn "── workhorse log (last 50 lines) ──"
    $COMPOSE logs --tail 50 workhorse 2>&1 || true
    fail "no DCUtR events — hole punching not exercised"
fi

# ── Step 9: Verify tunnel uses direct connection ─────────────────────

TUNNEL_DIRECT=$(echo "$CLEAN_LOGS" | grep "spawning tunnel" | grep "relayed=false" | wc -l | tr -d ' ')

if [[ "$TUNNEL_DIRECT" -gt 0 ]]; then
    pass "tunnel spawned on direct connection (relayed=false)"
else
    TUNNEL_RELAY=$(echo "$CLEAN_LOGS" | grep "spawning tunnel" | grep "relayed=true" | wc -l | tr -d ' ')
    if [[ "$TUNNEL_RELAY" -gt 0 ]]; then
        warn "tunnel spawned on relay (relayed=true) — DCUtR may have completed after tunnel setup"
    fi
    warn "── client log (last 50 lines) ──"
    $COMPOSE logs --tail 50 client 2>&1 || true
    fail "tunnel not spawned on direct connection"
fi
```

**Step 2: Run E2E test (if podman available)**

Run: `bash scripts/e2e_compose_test.sh`

Expected: All steps pass including DCUtR hole-punch success and direct tunnel verification. If DCUtR fails in the container environment due to conntrack issues, this will surface as a test failure that needs investigation.

---

### Task 9: Final verification and commit

**Step 1: Run full test suite**

Run: `cargo test -p cli`

Expected: All unit tests, property tests, and integration tests pass.

**Step 2: Run clippy**

Run: `cargo clippy -p cli -- -D warnings`

Expected: No warnings.

**Step 3: Review changes**

Run: `git diff --stat`

Verify only the expected files are modified:
- `cli/src/protocol.rs`
- `cli/src/state/event.rs`
- `cli/src/state/command.rs`
- `cli/src/state/tunnel.rs`
- `cli/src/node/mod.rs`
- `cli/src/node/execute.rs`
- `scripts/e2e_compose_test.sh`
- `docs/plans/2026-02-16-dcutr-holepunch-proof-design.md` (already created)
- `docs/plans/2026-02-16-dcutr-holepunch-proof.md` (this plan)

**Step 4: Commit**

```bash
git add cli/src/protocol.rs cli/src/state/event.rs cli/src/state/command.rs \
    cli/src/state/tunnel.rs cli/src/node/mod.rs cli/src/node/execute.rs \
    scripts/e2e_compose_test.sh \
    docs/plans/2026-02-16-dcutr-holepunch-proof-design.md \
    docs/plans/2026-02-16-dcutr-holepunch-proof.md
git commit -m "feat: add DCUtR hole-punch timeout with relay fallback

- Add HolePunchTimeout event and AwaitHolePunch command
- HolePunchFailed now falls back to relay instead of dropping specs
- Event loop tracks per-peer holepunch deadlines (15s timeout)
- SpawnTunnel carries relayed flag for logging and E2E verification
- E2E test asserts DCUtR success and direct tunnel connection"
```
