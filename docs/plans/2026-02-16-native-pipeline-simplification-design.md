# Native Pipeline Simplification

## Problem

The current external address management is broken and overcomplicated:

1. `make_external_addr()` computes `external_ip:local_port` but NAT remaps ports — the DHT gets wrong addresses
2. `discover_external_ip()` calls ipify — an external HTTP dependency for something libp2p handles natively
3. `add_external_address()` suppresses `NewExternalAddrCandidate` for the same address, blocking DCUtR from learning correct NAT-mapped addresses
4. Relay reservation is eager (on bootstrap connect) regardless of NAT status
5. Three-phase state machine (Initializing/Discovering/Participating) exists to gate eager relay — unnecessary when relay is event-driven

## Solution

Let libp2p's native Identify → AutoNAT → DCUtR pipeline handle address discovery and NAT traversal. Remove manual address management. Flatten the state machine. Gate relay on AutoNAT.

## Native Address Pipeline (No Manual Intervention)

```
Peer connects to bootstrap
  → Identify exchange (automatic on every connection)
  → Bootstrap observes peer's NAT-mapped address (correct IP + correct port)
  → Identify emits NewExternalAddrCandidate(observed_addr)
    → DCUtR caches it (for hole-punching)
    → AutoNAT probes it (dial-back test)
      → Public: ExternalAddrConfirmed → Kademlia auto-switches to Server mode
      → Private: NatStatus::Private → request relay reservation
        → relay::client emits ExternalAddrConfirmed(circuit_addr)
        → Kademlia auto-switches to Server mode via circuit address
```

## Changes

### 1. Delete Manual Address Management

| Remove | Reason |
|--------|--------|
| `discover_external_ip()` | AutoNAT + Identify handle this |
| `make_external_addr()` | Computes wrong port; Identify gets the right one |
| `--external-address` CLI flag | No longer needed |
| `reqwest` dependency | Only used for ipify |
| `NewListenAddr → make_external_addr` in event loop | Unnecessary |
| `NodeConfig.external_address` field | No longer needed |

Keep `is_valid_external_candidate()` — still useful for filtering bad `NewExternalAddrCandidate` events (circuit addrs, private IPs).

### 2. Bootstrap Server Detection

A node with empty `--bootstrap` is the bootstrap/relay server.

Bootstrap server special handling (official relay-server pattern):
```rust
// On Identify::Received, confirm the observed address
// (first node has nobody to AutoNAT it)
if config.bootstrap_addrs.is_empty() {
    if let Event::PeerIdentified { observed_addr, .. } = &event {
        swarm.add_external_address(observed_addr.clone());
    }
}
```

Non-bootstrap peers: zero manual address management.

### 3. Flatten Phase State Machine

**Before:** `Initializing → Discovering → Participating → ShuttingDown`

**After:** `Joining → Ready → ShuttingDown`

- `Joining`: not yet ready. Subsumes Initializing + Discovering.
- `Ready`: Kademlia bootstrapped AND has confirmed external address (direct or circuit).
- `ShuttingDown`: graceful exit.

**Transition guards:**
- `Joining → Ready`: `kad_bootstrapped && !external_addrs.is_empty()`
- `Ready → Joining`: last peer disconnects
- `* → ShuttingDown`: shutdown signal

### 4. Event-Driven Relay

**Before:** Relay requested eagerly on bootstrap connect via `maybe_enter_participating()`.

**After:** Relay requested only when `NatStatusChanged(Private)`:

```
NatStatusChanged(Private) + relay_reserved == false → RequestRelayReservation
NatStatusChanged(Public) + relay_reserved == true  → (optionally clear)
```

Replace `RelayState` enum with `relay_reserved: bool`.

### 5. State Shape

**Before:** `Phase × NatStatus × RelayState` (3 orthogonal coproducts)

**After:**
```rust
pub struct PeerState {
    pub phase: Phase,              // Joining | Ready | ShuttingDown
    pub nat_status: NatStatus,     // Unknown | Public | Private
    pub relay_reserved: bool,
    pub known_peers: HashMap<PeerId, HashSet<Multiaddr>>,
    pub bootstrap_peers: HashSet<PeerId>,
    pub kad_bootstrapped: bool,
    pub external_addrs: HashSet<Multiaddr>,
}
```

### 6. Event Changes

Add to `Event` enum:
- `PeerIdentified` gains `observed_addr: Multiaddr` field (from `identify::Info`)

Remove from `Event` enum:
- `NoBootstrapPeers` — replaced by bootstrap detection in event loop

Modify translate.rs:
- `Identify::Received` → include `observed_addr` in `PeerIdentified`

### 7. Identify Observed Address for Bootstrap

In translate.rs, the `Identify::Received` event now includes `observed_addr`:
```rust
BehaviourEvent::Identify(identify::Event::Received { peer_id, info, .. }) => {
    vec![Event::PeerIdentified {
        peer: *peer_id,
        listen_addrs: info.listen_addrs.clone(),
        observed_addr: info.observed_addr.clone(),
    }]
}
```

In the event loop (node/mod.rs), bootstrap nodes handle this:
```rust
if bootstrap_peer_ids.is_empty() {
    if let Event::PeerIdentified { observed_addr, .. } = &state_event {
        swarm.add_external_address(observed_addr.clone());
    }
}
```

### 8. Files Changed

| File | Change |
|------|--------|
| `external_addr.rs` | Delete `discover_external_ip`, `make_external_addr`, keep `is_valid_external_candidate` |
| `node/mod.rs` | Remove external IP discovery, NewListenAddr rewrite, add bootstrap Identify handler |
| `state/peer.rs` | Flatten Phase, replace RelayState with bool, gate relay on NatStatus |
| `state/event.rs` | Add `observed_addr` to PeerIdentified, remove NoBootstrapPeers |
| `state/command.rs` | No change |
| `state/app.rs` | Update for new Phase names |
| `state/tunnel.rs` | Update Phase references |
| `node/translate.rs` | Pass observed_addr through |
| `node/execute.rs` | No change (commands stay the same) |
| `bin/cli.rs` | Remove `--external-address` flag |
| `Cargo.toml` (cli) | Remove `reqwest` dependency |
| `Cargo.toml` (workspace) | Remove `reqwest` if unused elsewhere |

### 9. Test Impact

- Property tests for `PeerState` transitions: update for new Phase names and relay gating
- Property tests for `external_addr`: remove tests for deleted functions, keep candidate filtering tests
- Integration tests: update Phase assertions
- E2E tests: update compose env vars (remove PUNCHGATE_EXTERNAL_ADDRESS)

### 10. What Stays

- `is_valid_external_candidate()` + its tests — still filters bad candidates
- `NewExternalAddrCandidate` handling in event loop — still needed with the filter
- Kademlia, DCUtR, AutoNAT, Identify, Relay behaviours — all stay
- Tunnel state machine — unchanged
- Bootstrap reconnect backoff — unchanged
- Service publishing — unchanged
