# DCUtR Hole-Punch Proof-of-Concept Design

## Problem

The tunnel state machine has three bugs that prevent tunnels from working correctly when both peers are behind NAT:

1. `HolePunchFailed` drops tunnel specs permanently instead of falling back to relay
2. No timeout for `awaiting_holepunch` — specs can sit forever if DCUtR events don't arrive
3. No `HolePunchTimeout` event — the state machine cannot learn that time has passed

Additionally, the E2E test treats DCUtR as informational. We need to prove DCUtR hole-punching works end-to-end in the container topology with full-cone NAT.

## Root Cause Analysis

### Real-world failure (local ↔ bycur)

bycur's NAT is symmetric (endpoint-dependent mapping) — each connection to a different destination gets a different external port. DCUtR exchanges stale addresses that don't map to the hole-punch destination. Hole-punching is mathematically impossible with symmetric NAT.

### Tunnel deadlock sequence

1. bycur connects to local via relay → `relayed_peers.insert(bycur)`
2. DCUtR fires and fails (or event never arrives on dialer side)
3. Local transitions to Ready → DHT resolves bycur as provider
4. `route_connected_peer`: sees relayed → parks specs in `awaiting_holepunch`
5. No `HolePunchSucceeded` or `HolePunchFailed` arrives → specs stuck forever
6. Relayed connection drops → `ConnectionLost` → ERROR "all connections lost"

## Approach

**DCUtR-first with relay fallback after timeout.** Wait for DCUtR result. Spawn tunnel on direct connection if it succeeds. Fall back to relay on failure or timeout (15s).

## Design

### New event

```
Event::HolePunchTimeout { peer: PeerId }
```

### New command

```
Command::AwaitHolePunch { peer: PeerId }
```

Emitted by the tunnel state machine when specs enter `awaiting_holepunch`. The event loop uses this to start a per-peer deadline timer.

### New protocol constant

```
HOLEPUNCH_TIMEOUT: Duration = Duration::from_secs(15)
```

### SpawnTunnel change

Add `relayed: bool` field to `Command::SpawnTunnel` for logging and E2E verification:

```
Command::SpawnTunnel { peer, service, bind, relayed }
```

### Tunnel state machine changes (tunnel.rs)

**`route_connected_peer` decision tree:**

```
if holepunch_failed.contains(peer):
    SpawnTunnel { relayed: true }    // relay fallback
elif relayed_peers.contains(peer):
    awaiting_holepunch.insert(peer)  // wait for DCUtR
    emit AwaitHolePunch { peer }     // start timeout
else:
    SpawnTunnel { relayed: false }   // direct connection
```

**Changed transitions:**

| Event | Old | New |
|-------|-----|-----|
| `HolePunchFailed` | Drop specs, log error | Spawn on relay (`relayed: true`) |
| `HolePunchTimeout` | N/A | Spawn on relay (`relayed: true`) |

**Unchanged transitions (correct as-is):**

| Event | Behavior |
|-------|----------|
| `HolePunchSucceeded` | Spawn from `awaiting_holepunch` (`relayed: false`) |
| `TunnelPeerConnected { relayed: false }` | Spawn from both maps (`relayed: false`) |
| `ConnectionLost { remaining: 0 }` | Clean up, log error for lost specs |

### Event loop changes (node/mod.rs)

**New state:**

```rust
let mut holepunch_deadlines: HashMap<PeerId, tokio::time::Instant> = HashMap::new();
```

**Execute `AwaitHolePunch` command:**

```rust
Command::AwaitHolePunch { peer } => {
    holepunch_deadlines.entry(peer)
        .or_insert(tokio::time::Instant::now() + HOLEPUNCH_TIMEOUT);
}
```

**New `select!` branch** (alongside existing reconnect timer):

When the next deadline fires, synthesize `Event::HolePunchTimeout { peer }` and feed it through the state machine.

**Clear deadlines** when processing events:
- `HolePunchSucceeded { remote_peer }`
- `HolePunchFailed { remote_peer }`
- `TunnelPeerConnected { peer, relayed: false }`
- `ConnectionLost { peer, remaining: 0 }`

### E2E test changes (e2e_compose_test.sh)

Step 8 becomes a hard assertion:

- Assert `"hole punch succeeded"` appears in client OR workhorse logs
- Assert `"spawning tunnel"` with `relayed=false` in client logs (proves direct connection)
- Fail the test if DCUtR did not succeed

### Key property

No tunnel specs are ever permanently lost. Every path through the state machine either spawns the tunnel (direct or relay) or logs an error on connection loss.

## Files to modify

| File | Change |
|------|--------|
| `cli/src/state/event.rs` | Add `HolePunchTimeout { peer }` variant |
| `cli/src/state/command.rs` | Add `AwaitHolePunch { peer }`, add `relayed: bool` to `SpawnTunnel` |
| `cli/src/state/tunnel.rs` | Implement timeout/fallback transitions, emit `AwaitHolePunch` |
| `cli/src/node/mod.rs` | Holepunch deadline tracking, new `select!` branch, clear deadlines |
| `cli/src/node/execute.rs` | Handle `AwaitHolePunch` command, update `SpawnTunnel` logging |
| `cli/src/protocol.rs` | Add `HOLEPUNCH_TIMEOUT` constant |
| `scripts/e2e_compose_test.sh` | Hard assertion for DCUtR success |
| `cli/tests/integration.rs` | Update tests for new `SpawnTunnel { relayed }` field |
