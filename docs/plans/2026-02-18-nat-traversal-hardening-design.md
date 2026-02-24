# NAT Traversal Hardening — Design

## Problem

Production logs show both peers behind home routers with Endpoint-Independent Mapping but Address-Restricted Filtering. DCUtR's QUIC handshake packets arrive at each NAT from unknown source IPs and get dropped — 3 dial timeouts, hole-punch fails, falls back to relay.

Additionally, port mapping (UPnP/NAT-PMP/PCP) fails for two reasons:
- CGNAT: UPnP gateway returns `100.64.x.x` (shared address space), mapping is useless
- No protocol support: router doesn't implement any mapping protocol

The retry path is also broken: `HolePunchFailed` (DCUtR error) never schedules retries — only the 30s `HolePunchTimeout` does, but DCUtR reports failure in ~15s, so the timeout never fires.

## Approach: Layered Defense

Three independent, composable fixes plus a bug fix:

1. **CGNAT/private IP guard** — reject non-routable addresses from port mapping
2. **NAT priming** — send UDP packets to peer's observed address before DCUtR
3. **Primed direct dial on retry** — bypass DCUtR on retry, dial directly after priming
4. **Fix retry gap** — `HolePunchFailed` also schedules retries

## Fix 1: CGNAT/Private IP Guard

### Location
`cli/src/port_mapping.rs`

### Design
Add `is_publicly_routable(ip: IpAddr) -> bool` that rejects:
- `100.64.0.0/10` — CGNAT (RFC 6598)
- `10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16` — private (RFC 1918)
- `169.254.0.0/16` — link-local
- `127.0.0.0/8` — loopback

Check after UPnP's `get_external_ip()` and after NAT-PMP/PCP returns external IP. Reject with clear log message before attempting `add_port`.

### State Machine Impact
None. Pure validation in `port_mapping.rs`.

## Fix 2: NAT Priming

### Concept
When a relayed connection is established with a hole-punch target, immediately send UDP packets from the local QUIC socket's address to the peer's observed external address. This creates an outbound NAT mapping so the peer's incoming DCUtR QUIC packets are recognized as return traffic.

### Socket Strategy
Bind a fresh `tokio::net::UdpSocket` with `SO_REUSEADDR`/`SO_REUSEPORT` to the same `(local_ip, local_port)` that the QUIC transport listens on. Packets from this socket create the same NAT mapping because the NAT sees identical `(src_ip, src_port)`.

### Trigger
New command `Command::PrimeNatMapping { peer, peer_addrs }` emitted alongside `AwaitHolePunch` when:
- Peer is in `relayed_peers`
- NAT mapping is hole-punch viable

### Execution
1. Extract peer's observed external `/ip4/X/udp/P` from `peer_addrs`
2. Find local QUIC listen address from `swarm.listeners()`
3. Bind UDP socket with `SO_REUSEPORT` to local address
4. Send 3 UDP packets (`b"punch"`) to peer's external address
5. Fire-and-forget — packets create NAT mapping, will be dropped by peer

### Timing
Priming fires immediately on relayed connection. DCUtR negotiates over relay (~100-300ms), so NAT mapping is fresh when direct QUIC handshake starts.

### State Machine Changes
- New command: `Command::PrimeNatMapping { peer, peer_addrs: Vec<Multiaddr> }`
- `TunnelState::route_connected_peer` emits it alongside `AwaitHolePunch`

## Fix 3: Fix Retry Gap

### Bug
`HolePunchFailed` (DCUtR error after 3 dial attempts, ~15s) removes from `holepunch_deadlines` but never inserts into `holepunch_retries`. Only `HolePunchTimeout` (30s external timer) schedules retries. Since DCUtR fails before 30s, the timeout never fires, retries never happen.

### Fix
In event loop's `HolePunchFailed` handler, also insert into `holepunch_retries` with the standard 60s retry interval.

## Fix 4: Primed Direct Dial on Retry

### Concept
When `HolePunchRetryTick` fires (60s after failure), instead of re-dialing through relay (which re-triggers the same DCUtR flow), send priming packets then dial the peer's observed external address directly.

### Peer Address Tracking
New `HashMap<PeerId, Vec<Multiaddr>>` in the event loop, populated from `PeerIdentified` events' observed addresses. Passed through commands so `TunnelState` and `execute_commands` have access.

### State Machine Changes
- New command: `Command::PrimeAndDialDirect { peer, peer_addrs: Vec<Multiaddr> }`
- `HolePunchRetryTick` handler emits `PrimeAndDialDirect` instead of `RetryDirectDial`

### Execution
1. Send NAT priming packets (same as Fix 2)
2. Extract peer's external multiaddr (non-relay, non-circuit)
3. `swarm.dial()` to the external address directly

### Why This Helps
If both peers retry simultaneously (both have 60s timers from roughly the same failure time), both send priming packets at ~the same moment. Each side's priming creates an outbound NAT mapping. When the direct QUIC dial arrives shortly after, the NAT recognizes it as return traffic for the priming packet's mapping.

## Files Modified

| File | Changes |
|------|---------|
| `cli/src/port_mapping.rs` | Add `is_publicly_routable()`, validate external IPs |
| `cli/src/state/command.rs` | Add `PrimeNatMapping`, `PrimeAndDialDirect` commands |
| `cli/src/state/tunnel.rs` | Emit priming commands, change retry command |
| `cli/src/node/mod.rs` | Retry on `HolePunchFailed`, peer addr tracking, priming execution |
| `cli/src/node/execute.rs` | Handle new commands |

## Not Included

- **Birthday paradox multi-port probing** — both production peers have cone NATs, not symmetric. This technique only helps symmetric NAT.
- **Filtering-aware state machine** — filtering probe result stays informational (logging only). Adding it to state machine would require knowing remote peer's type too.
- **Cross-peer role alternation** — would require new protocol messages. Direct dial after priming achieves similar effect more simply.
