# NAT STUN Diagnostics & Hole-Punch Viability

## Problem

Three-node deployment (bootstrap + 2 NATed peers) shows DCUtR hole-punch failing every time:

- **Client**: `hole punch timeout, falling back to relay` after 15s
- **Workhorse**: `hole punch failed: Giving up after 3 dial attempts`
- **Both**: `nat=undetermined (single observer per IP)`

Every tunnel pays a 15-second penalty before falling back to relay — the system cannot determine whether hole-punching is even possible.

## Root Causes

### 1. NAT type unknown — single observer

Only one bootstrap node exists, providing one Identify observation per NATed peer. The symmetric NAT detection at `node/mod.rs:244` requires ≥2 observations from different IPs to compare external port mappings. With a single observer, NAT type is always "undetermined."

### 2. Relay-observed addresses corrupt NAT tracking

`PeerIdentified` events from relay-connected peers report the relay's address as `observed_addr`, not the actual peer's NAT-mapped address. On bycur's node:

```
NAT mapping observed observer=<CLIENT> observed_ip=81.219.135.164 observed_port=4001
```

`81.219.135.164:4001` is the bootstrap/relay — not the client's real address (`185.235.206.23:11542`). The code at `node/mod.rs:239-259` adds this to the observation tracking without checking whether the observer's connection is relayed.

### 3. DCUtR attempts doomed hole-punches

When NAT is symmetric (Address-Dependent Mapping), the external port changes per destination. DCUtR exchanges candidates based on bootstrap-observed ports, but the actual ports used when peers dial each other differ. Hole-punching cannot succeed, yet the system waits 15 seconds before falling back to relay.

## Design

### Part 1: STUN-based NAT mapping detection

**New module**: `cli/src/nat_probe.rs`

On startup, when `--bootstrap` is specified (peer is behind NAT), probe 2 public STUN servers to classify NAT mapping behavior.

**Dependency**: `stun` crate (from webrtc-rs) for STUN message construction and XOR-MAPPED-ADDRESS parsing. `tokio::net::UdpSocket` for transport.

**Types**:

```rust
pub enum NatMapping {
    EndpointIndependent { external_addr: SocketAddr },
    AddressDependent,
}
```

**Flow**:

1. Bind temporary `UdpSocket` to `0.0.0.0:0`
2. Send STUN Binding Request to `stun.l.google.com:19302`
3. Parse response → `(external_ip_A, external_port_A)`
4. Send STUN Binding Request to `stun.cloudflare.com:3478` from same socket
5. Parse response → `(external_ip_B, external_port_B)`
6. If `port_A == port_B` → `EndpointIndependent` (cone NAT, hole-punch viable)
7. If `port_A != port_B` → `AddressDependent` (symmetric NAT, relay only)

**Integration**: Called from `node::run()` before swarm construction. Result logged and fed into state machine as `Event::NatMappingDetected(NatMapping)`.

**Default STUN servers**: `stun.l.google.com:19302`, `stun.cloudflare.com:3478`. Configurable via `--stun-server` flag.

**Error handling**: STUN unreachable → log warning, proceed with `Unknown` mapping (existing behavior).

### Part 2: Filter relay-observed addresses from NAT tracking

**Change in**: `cli/src/node/mod.rs`

Maintain a `HashSet<PeerId>` of peers connected via relay in the node event loop. Populated on `ConnectionEstablished` when endpoint contains `/p2p-circuit`, cleared on `ConnectionClosed`.

Before adding NAT observations from `PeerIdentified`, check if the observer's connection is relayed:

```rust
if !relayed_connections.contains(&peer) {
    // ... existing observation logic
}
```

This prevents relay addresses from polluting the NAT port tracking.

### Part 3: Skip doomed hole-punches for symmetric NAT

**New field**: `PeerState.nat_mapping: Option<NatMapping>`

**State machine change**: New event `NatMappingDetected(NatMapping)` sets `nat_mapping` on `PeerState`.

**Routing change in `TunnelState`**: When a relayed peer is connected and NAT mapping is `AddressDependent`, skip `AwaitHolePunch` and spawn tunnel on relay immediately. No 15-second timeout penalty.

**Logging**:

```
WARN NAT type: symmetric (Address-Dependent Mapping)
WARN hole-punching not viable, all tunnels will use relay transport
```

or:

```
INFO NAT type: cone (Endpoint-Independent Mapping)
INFO hole-punching viable, external address: 185.235.206.23:11542
```

## Scope

### In scope

- STUN-based NAT mapping detection using `stun` crate
- Filter relay-observed addresses from NAT tracking
- Skip hole-punch when symmetric NAT detected
- Startup logging of NAT type and hole-punch viability

### Out of scope

- NAT filtering behavior detection (EIF/ADF/APDF) — mapping type is sufficient for hole-punch viability
- Runtime re-detection if NAT changes (IP rebind)
- Container/integration tests for different NAT types (future work)
- `--stun-server` CLI flag (hardcoded defaults for now, add flag later if needed)
