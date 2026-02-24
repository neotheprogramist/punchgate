# UPnP/NAT-PMP/PCP Port Mapping Design

## Problem

Both peers behind home routers show Endpoint-Independent Mapping (STUN-verified) but Address-Restricted Filtering. DCUtR hole-punching fails because both NATs drop unsolicited inbound UDP from unknown source IPs. The relay fallback works but adds latency.

## Solution

Request explicit port forwarding from the router via UPnP IGD, NAT-PMP, or PCP. If any protocol succeeds, the NAT opens a port unconditionally for inbound traffic, eliminating the filtering problem. If all fail, the existing relay path handles connectivity.

## Design Decisions

- **Two-crate split**: `igd-next` for UPnP, `crab_nat` for NAT-PMP/PCP
- **Dynamic port**: Keep random OS-assigned QUIC listen port, detect it after `listen_on()`, then request mapping for that port
- **Skip relay if mapped**: Successful port mapping emits `NatStatusChanged(Public)`, which prevents relay reservation
- **Map once, no renewal**: Request mapping at startup, trust it persists. UPnP 0-TTL mappings survive until router reboot. NAT-PMP/PCP mappings expire at their natural TTL
- **5-second timeout**: All three protocols race in parallel, first success wins

## Module: `cli/src/port_mapping.rs`

### Types

```rust
pub struct PortMapping {
    pub external_addr: SocketAddr,
    pub protocol_used: &'static str, // "UPnP" | "NAT-PMP" | "PCP"
}
```

### Entry Point

```rust
pub async fn acquire_port_mapping(internal_port: u16) -> Option<PortMapping>
```

### Flow

1. Detect default gateway IP via `default-net` crate
2. Race three futures with `tokio::select!`:
   - UPnP: `igd_next::aio::tokio::search_gateway()` then `gateway.add_port(UDP, internal_port, local_addr, 0, "punchgate")`
   - NAT-PMP/PCP: `crab_nat::PortMapping::new(gateway, local_addr, UDP, internal_port, opts)` (tries PCP first, falls back to NAT-PMP)
3. Wrap in 5-second `tokio::time::timeout`
4. Return first success or `None`
5. Log each protocol's outcome at info/warn level

## Integration with Startup Flow

### Current

```
STUN probe → build swarm → listen → dial bootstrap → PeerIdentified → request relay → Ready
```

### New

```
STUN probe → build swarm → listen → detect listen port → spawn port_mapping task
                                  → dial bootstrap → PeerIdentified →
  if port_mapping succeeded:
    add_external_address(mapped_addr) → NatStatusChanged(Public) → skip relay → Ready
  else:
    request relay (existing flow) → Ready
```

### Changes in `node/mod.rs`

1. After `swarm.listen_on()`, capture the QUIC listen port from `SwarmEvent::NewListenAddr`
2. Spawn `acquire_port_mapping(listen_port)` as background task, store `JoinHandle`
3. Before `PeerIdentified` triggers relay request, check if port mapping completed:
   - Success: `swarm.add_external_address()` with mapped addr, emit `NatStatusChanged(Public)`
   - Failure: proceed with existing relay flow

### State Machine Impact

No new events or commands needed. Successful port mapping emits `NatStatusChanged(Public)`, which the existing state machine handles — it prevents `RequestRelayReservation` commands.

## Dependencies

```toml
igd-next = { version = "0.16", features = ["aio_tokio"] }
crab_nat = "0.7"
default-net = "0.23"
```

- `igd-next`: UPnP IGD client with async tokio support. Used by Veilid in production.
- `crab_nat`: Pure Rust NAT-PMP (RFC 6886) and PCP (RFC 6887) client. Tokio-native.
- `default-net`: Cross-platform default gateway detection (Linux, macOS, Windows).

## Error Handling

All port mapping failures are non-fatal. `acquire_port_mapping` returns `Option<PortMapping>`. Individual protocol failures logged at `warn` level. The relay fallback path handles all connectivity if port mapping is unavailable.

## Diagnostics

On success:
```
INFO port mapping acquired via UPnP: 203.0.113.5:4001 (internal port 55606)
```

On failure:
```
WARN UPnP gateway search failed: timeout after 5s
WARN NAT-PMP mapping failed: gateway 192.168.1.1 refused request
INFO no port mapping available, falling back to relay
```
