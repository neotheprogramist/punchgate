# Reactive Architecture Refactoring

Monadic Mealy machine composition with type-safe newtypes and modular structure.

---

## Goals

1. **Fully reactive** — one composed state machine, trivial event loop, zero mutable state outside the coalgebra
2. **Type safety** — newtypes replace stringly-typed data (`ServiceName`, `KademliaKey`, `HostAddr`)
3. **Modular** — split 1300-line `state.rs` and 1100-line `node.rs` into focused submodules
4. **Trait abstractions** — `MealyMachine` trait enables composition and independent testing
5. **Test cleanup** — remove duplicated/trivial tests, consolidate overlapping roundtrips, share generators
6. **No dynamic types** — zero `Box<dyn>`, zero `as` casts (already clean, maintain this)

---

## Architecture: Monadic Machine Composition

Two Mealy machines composed via monadic sequencing. The inner machine (PeerState) runs first; its phase delta becomes a bridge event for the outer machine (TunnelState). Commands accumulate in deterministic order.

```
Event ──→ PeerState.transition(event)
              │
              ├──→ (PeerState', Vec<Command>)
              │           │
              │    phase delta → Event::PhaseChanged { old, new }
              │           │
              ▼           ▼
         TunnelState.transition(PhaseChanged)
         TunnelState.transition(event)
              │
              ├──→ (TunnelState', Vec<Command>)
              │
              ▼
         Final: PeerCmds ++ TunnelCmds  (deterministic order)
```

### AppState Composition

```rust
pub struct AppState {
    peer: PeerState,
    tunnel: TunnelState,
}

impl MealyMachine for AppState {
    type Event = Event;
    type Command = Command;

    fn transition(self, event: Event) -> (Self, Vec<Command>) {
        let old_phase = self.peer.phase;
        let (peer, peer_cmds) = self.peer.transition(event.clone());

        let bridge = match old_phase == peer.phase {
            true => None,
            false => Some(Event::PhaseChanged { old: old_phase, new: peer.phase }),
        };

        let (tunnel, tunnel_cmds) = bridge
            .into_iter()
            .chain(std::iter::once(event))
            .fold(
                (self.tunnel, Vec::new()),
                |(state, mut cmds), evt| {
                    let (s, c) = state.transition(evt);
                    cmds.extend(c);
                    (s, cmds)
                },
            );

        let cmds = peer_cmds.into_iter().chain(tunnel_cmds).collect();
        (Self { peer, tunnel }, cmds)
    }
}
```

---

## MealyMachine Trait

```rust
pub trait MealyMachine: Sized {
    type Event;
    type Command;
    fn transition(self, event: Self::Event) -> (Self, Vec<Self::Command>);
}
```

Both `PeerState` and `TunnelState` implement this with `Event = Event, Command = Command`. Testable independently, composable monadically.

---

## Newtypes

### ServiceName

Replaces `String` in 4 structs, 5 HashMaps, eliminates 12 `.clone()` calls via `Arc<str>`.

```rust
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ServiceName(Arc<str>);
```

### KademliaKey

Replaces `Vec<u8>` in 4 Command variants. Prevents passing arbitrary bytes where formatted keys are expected.

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KademliaKey(Vec<u8>);

impl KademliaKey {
    pub fn for_service(name: &ServiceName) -> Self { ... }
    pub fn into_record_key(self) -> kad::RecordKey { ... }
}
```

### HostAddr

Replaces `ServiceAddr.host: String` and the `host.contains(':')` IPv6 detection hack.

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostAddr {
    V4(Ipv4Addr),
    V6(Ipv6Addr),
    Domain(String),
}
```

---

## TunnelState Machine

Absorbs the 6 mutable HashMaps from `node.rs`:

```rust
pub struct TunnelState {
    pending_by_peer: HashMap<PeerId, Vec<(ServiceName, ServiceAddr)>>,
    pending_by_service: HashMap<ServiceName, Vec<ServiceAddr>>,
    dht_peer_queries: HashMap<QueryId, PeerId>,
    dht_service_queries: HashMap<QueryId, ServiceName>,
    dialing: HashSet<PeerId>,
    services_published: bool,
}
```

Reacts to:
- `PhaseChanged` (bridge event) — initiates DHT lookups and publishes services on entering Participating
- `DhtPeerLookupComplete` — dial peer or spawn tunnel if already connected
- `DhtProvidersFound` / `DhtProvidersFailed` — resolve service name to peer
- `TunnelPeerConnected` — spawn tunnels for waiting specs
- `HolePunchSucceeded` / `HolePunchFailed` — spawn or abort relayed tunnels

Emits tunnel-specific commands:
- `DhtLookupPeer { peer }`
- `DhtGetProviders { key }`
- `DialPeer { peer }`
- `SpawnTunnel { peer, service, bind }`
- `PublishServices`

---

## Unified Event Enum

```rust
pub enum Event {
    // Network lifecycle (PeerState)
    ListeningOn { addr: Multiaddr },
    BootstrapConnected { peer: PeerId, addr: Multiaddr },
    ConnectionLost { peer: PeerId, remaining_connections: u32 },
    NatStatusChanged(NatStatus),
    ExternalAddrConfirmed { addr: Multiaddr },
    ExternalAddrExpired { addr: Multiaddr },
    MdnsDiscovered { peers: Vec<(PeerId, Multiaddr)> },
    MdnsExpired { peers: Vec<(PeerId, Multiaddr)> },
    PeerIdentified { peer: PeerId, listen_addrs: Vec<Multiaddr> },
    KademliaBootstrapOk,
    KademliaBootstrapFailed { reason: String },
    RelayReservationAccepted { relay_peer: PeerId },
    RelayReservationFailed { relay_peer: PeerId, reason: String },
    DiscoveryTimeout,
    NoBootstrapPeers,
    ShutdownRequested,

    // Tunnel lifecycle (TunnelState)
    HolePunchSucceeded { remote_peer: PeerId },
    HolePunchFailed { remote_peer: PeerId, reason: String },
    TunnelPeerConnected { peer: PeerId, relayed: bool },
    DhtPeerLookupComplete { query_id: QueryId, peer: PeerId, connected: bool },
    DhtProvidersFound { query_id: QueryId, service: ServiceName, providers: Vec<PeerId> },
    DhtProvidersFailed { query_id: QueryId, service: ServiceName, reason: String },

    // Bridge (synthesized by AppState)
    PhaseChanged { old: Phase, new: Phase },
}
```

---

## Unified Command Enum

```rust
pub enum Command {
    // Network (from PeerState)
    Dial(Multiaddr),
    Listen(Multiaddr),
    KademliaBootstrap,
    KademliaAddAddress { peer: PeerId, addr: Multiaddr },
    RequestRelayReservation { relay_peer: PeerId, relay_addr: Multiaddr },
    AddExternalAddress(Multiaddr),
    Shutdown,

    // Tunnel (from TunnelState)
    DhtLookupPeer { peer: PeerId },
    DhtGetProviders { key: KademliaKey },
    DialPeer { peer: PeerId },
    SpawnTunnel { peer: PeerId, service: ServiceName, bind: ServiceAddr },
    PublishServices,
}
```

Removed: `Command::Log` (derived from state deltas), `KademliaStartProviding` / `KademliaPutRecord` / `KademliaGetProviders` (absorbed into `PublishServices` and `DhtGetProviders`).

---

## Logging: Derived from State Deltas

Replace `Command::Log` with delta-based observability in the event loop:

```rust
#[derive(PartialEq)]
struct StateSummary {
    phase: Phase,
    nat: NatStatus,
    relay_active: bool,
    peer_count: usize,
}

fn log_transition(old: &StateSummary, event: &Event, new: &StateSummary) {
    if old.phase != new.phase {
        tracing::info!(event = %event, from = %old.phase, to = %new.phase, "phase transition");
    }
    if old.nat != new.nat {
        tracing::info!(old = %old.nat, new = %new.nat, "NAT status changed");
    }
    // ... relay, peer count deltas
}
```

Eliminates: `Command::Log` variant, ~50 lines of `commands.push(Command::Log(...))`, `event_name()` (22 arms), `phase_name()` (4 arms).

Requires: `strum::Display` derive on `Event`, `Phase`, `NatStatus`.

---

## Module Structure

```
cli/src/
├── lib.rs                      # Module declarations + re-exports
├── bin/cli.rs                  # Entry point (unchanged)
├── traits.rs                   # MealyMachine trait
├── types.rs                    # ServiceName, KademliaKey, HostAddr
├── specs.rs                    # ServiceAddr, ExposedService, TunnelSpec, TunnelByNameSpec
├── protocol.rs                 # Constants (unchanged)
├── identity.rs                 # Keypair I/O (unchanged)
├── shutdown.rs                 # Signal handling (unchanged)
├── behaviour.rs                # libp2p NetworkBehaviour (unchanged)
├── tunnel.rs                   # Wire protocol: codec, accept_loop, connect_tunnel
├── state/
│   ├── mod.rs                  # Re-exports
│   ├── event.rs                # Event enum
│   ├── command.rs              # Command enum
│   ├── peer.rs                 # PeerState + impl MealyMachine
│   ├── tunnel.rs               # TunnelState + impl MealyMachine
│   └── app.rs                  # AppState composition
├── node/
│   ├── mod.rs                  # NodeConfig + run()
│   ├── translate.rs            # SwarmEvent → Vec<Event>
│   ├── execute.rs              # Command → swarm calls
│   └── backoff.rs              # ReconnectBackoff
└── test_utils.rs               # cfg(test): shared generators
```

---

## Test Cleanup

### Remove (4 tests)
- `service.rs::parse_expose_roundtrip` (duplicates `exposed_service_roundtrip`)
- `service.rs::parse_tunnel_by_name_roundtrip` (duplicates `tunnel_by_name_spec_roundtrip`)
- `tunnel.rs::parse_tunnel_spec_roundtrip` (duplicates `tunnel_spec_roundtrip`)
- `protocol.rs::protocol_strings_are_valid` (trivial, no assertions)

### Consolidate (6 → 2 tests)
- 3 `service_addr_*_roundtrip` → 1 `service_addr_roundtrip` with `arb_host_addr()` generator
- 2 `exposed_service_*_roundtrip` → 1 `exposed_service_roundtrip` with `arb_host_addr()`
- 2 `tunnel_spec_*_roundtrip` → 1 `tunnel_spec_roundtrip` with `arb_host_addr()`

### Shared generators (`test_utils.rs`)
Deduplicate from 3 files:
- `arb_peer_id()`, `arb_multiaddr()`, `arb_nonloopback_multiaddr()`
- `arb_service_name()`, `arb_host_addr()`, `arb_service_addr()`
- `arb_phase()`, `arb_nat_status()`, `arb_relay_state()`

### New tests for TunnelState
- `phase_changed_to_participating_initiates_lookups`
- `dht_providers_found_spawns_tunnel_when_connected`
- `dht_providers_found_dials_when_disconnected`
- `hole_punch_succeeded_spawns_waiting_tunnels`
- `hole_punch_failed_removes_pending`
- `tunnel_peer_connected_direct_spawns_immediately`
- `tunnel_peer_connected_relayed_waits_for_holepunch`

---

## Implementation Phases

### Phase 1: Foundation
- Add `strum` dependency
- Create `traits.rs` with `MealyMachine` trait
- Create `types.rs` with `ServiceName`, `KademliaKey`, `HostAddr` newtypes
- Create `test_utils.rs` with shared generators
- Remove 4 duplicate/trivial tests

### Phase 2: Specs consolidation
- Create `specs.rs` from `service.rs` parsing types + `tunnel.rs` spec types
- Replace `String` → `ServiceName`, `String` host → `HostAddr` throughout specs
- Consolidate 6 overlapping roundtrip tests → 2 parameterized tests
- Update `tunnel.rs` to use `ServiceName` in wire types

### Phase 3: State module split
- Create `state/event.rs` — unified Event enum with bridge + tunnel variants
- Create `state/command.rs` — unified Command enum (no Log variant)
- Create `state/peer.rs` — PeerState extracted from `state.rs`, impl MealyMachine
- Create `state/tunnel.rs` — TunnelState (new), impl MealyMachine
- Create `state/app.rs` — AppState monadic composition
- Add `strum::Display` to Event, Phase, NatStatus
- Migrate existing PeerState property tests, add TunnelState tests

### Phase 4: Node module split
- Create `node/translate.rs` — SwarmEvent → Vec<Event> (including new tunnel events)
- Create `node/execute.rs` — unified command executor (PeerCmd + TunnelCmd)
- Create `node/backoff.rs` — ReconnectBackoff (extracted)
- Rewrite `node/mod.rs` — trivial event loop: translate → transition → log_delta → execute
- Delta-based logging replaces Command::Log
- Remove all tunnel HashMap state from event loop

### Phase 5: Integration verification
- Run full test suite
- Run container E2E tests
- Verify all integration tests pass with new architecture
