# NAT Hole-Punch Diagnostics Design

## Problem

Hole-punching fails between two NATed peers despite both showing Endpoint-Independent NAT mapping via STUN. Three interrelated issues prevent diagnosis:

1. **STUN tests mapping but not filtering** — the probe classifies mapping behavior (same port across destinations) but not filtering behavior (who can send to the mapped port). Endpoint-Independent mapping with restricted filtering still blocks hole-punches.
2. **15s timeout races with DCUtR's 3-attempt cycle** — DCUtR takes ~15-18s for 3 QUIC handshake attempts. The custom `HOLEPUNCH_TIMEOUT` fires 0.3s before DCUtR reports its own result, so the initiator never receives the failure reason.
3. **No visibility into DCUtR address exchange** — when hole-punch fails, logs show the failure but not what addresses DCUtR exchanged or attempted to dial.

## Root Cause Analysis (from production logs)

Three peers: bootstrap (public), client (NAT at 185.235.206.23), workhorse (NAT at 81.219.135.162).

| Observation | Implication |
|---|---|
| STUN shows same port across 2 servers for both peers | Mapping is Endpoint-Independent (correct) |
| Client timeout at T+15.0s, workhorse DCUtR failure at T+15.3s | Timeout races with DCUtR — client misses failure reason |
| Workhorse DCUtR: "Giving up after 3 dial attempts" | All 3 simultaneous dial attempts failed |
| Both peers behind consumer home routers | Likely address-restricted or port-restricted cone NAT |

The most probable root cause: both NATs have Endpoint-Independent mapping but restricted filtering. Packets from unknown sources are dropped before the other side creates a NAT pinhole, causing all 3 DCUtR attempts to fail.

## Design

### Component 1: STUN Filtering Probe

Test whether the NAT allows incoming packets from arbitrary sources (filtering behavior).

**Protocol:**

1. NATed peer binds a fresh UDP socket, sends STUN binding request to learn its mapped external address
2. NATed peer sends `NatProbeRequest { mapped_addr }` to bootstrap via libp2p-stream
3. Bootstrap opens a fresh UDP socket and sends a probe packet to `mapped_addr`
4. Bootstrap responds with `NatProbeResponse { sent: true }`
5. NATed peer waits up to 3s for the UDP probe to arrive on its socket
6. Probe arrives → `EndpointIndependent` filtering (full cone)
7. Probe does not arrive → `Restricted` filtering

**New types in `nat_probe.rs`:**

```rust
pub enum NatFiltering {
    EndpointIndependent,  // Full cone — any host can reach mapped port
    Restricted,           // Address or port restricted — only known peers
    Unknown,              // Probe failed or no bootstrap
}

pub struct NatClassification {
    pub mapping: NatMapping,
    pub filtering: NatFiltering,
}
```

**Constraints:**

- Run filtering probe only when `mapping == EndpointIndependent` (symmetric NAT already precludes hole-punch)
- Bootstrap needs a UDP socket separate from QUIC to avoid header interference
- Communicate probe address via existing libp2p-stream protocol with new message type
- Timeout of 3s per probe attempt; single attempt is sufficient for classification

**Startup log output:**

```
nat_mapping=Endpoint-Independent nat_filtering=Restricted → hole-punch may fail (restricted filtering)
nat_mapping=Endpoint-Independent nat_filtering=EndpointIndependent → hole-punch should succeed
```

### Component 2: DCUtR Address Audit

Add info-level logging at key points in the hole-punch pipeline. No protocol changes.

**Log points:**

1. **External addresses at hole-punch start** — when `Command::AwaitHolePunch` fires, log the full set of `swarm.external_addresses()`. Snapshot of what this peer thinks its addresses are.

2. **Dial attempts during hole-punch** — `SwarmEvent::Dialing` events that occur while a peer is in `awaiting_holepunch` state, logged at `info` level with target address.

3. **Connection errors during hole-punch** — `SwarmEvent::OutgoingConnectionError` events during active hole-punch, logged at `info` level (currently `debug`) with specific transport error.

**Example output:**

```
holepunch_addresses peer=X addrs=[/ip4/185.235.206.23/udp/14176/quic-v1]
holepunch_dialing peer=X target=/ip4/81.219.135.162/udp/54097/quic-v1
holepunch_dial_error peer=X target=/ip4/81.219.135.162/udp/54097/quic-v1 error="QUIC handshake timeout"
```

### Component 3: Timeout Fix + Timing Correlation

**Timeout change:**

Bump `HOLEPUNCH_TIMEOUT` from 15s to 30s in `protocol.rs`. DCUtR's 3 QUIC handshake attempts take ~15-18s. The 30s deadline becomes a safety net for when DCUtR hangs entirely, rather than a race condition.

**Timing logs:**

Add timestamped logs at hole-punch lifecycle events for cross-peer correlation:

```
holepunch_start peer=X timestamp=2026-02-18T11:07:34.745Z
holepunch_dial_attempt peer=X attempt=1 target=... timestamp=...
holepunch_result peer=X outcome=failed reason="..." timestamp=...
```

Comparing timestamps across two peers' logs reveals dial simultaneity and relay coordination skew.

## Diagnostic Workflow

1. Build and deploy modified code to all 3 peers
2. Run the tunnel-by-name flow as before
3. Check startup logs for NAT classification: mapping + filtering
4. If filtering is `Restricted`: hole-punch failure explained — NATs drop packets from unknown sources before pinholes are created
5. Check address audit: confirm DCUtR exchanged correct external addresses
6. Check timing correlation: verify dial attempts were sufficiently simultaneous (< 1s offset)
7. Based on findings, choose fix strategy:
   - **Full cone filtering**: investigate code bug (addresses, timing)
   - **Restricted filtering + good simultaneity**: NAT filtering is the blocker — consider port prediction, longer retry, or accept relay
   - **Restricted filtering + poor simultaneity**: improve DCUtR coordination timing through relay

## Files Modified

| File | Change |
|---|---|
| `cli/src/nat_probe.rs` | Add `NatFiltering`, `NatClassification`, filtering probe logic |
| `cli/src/protocol.rs` | Bump `HOLEPUNCH_TIMEOUT` to 30s, add NAT probe stream protocol ID |
| `cli/src/node/mod.rs` | Run filtering probe at startup, add address audit + timing logs |
| `cli/src/node/translate.rs` | Enhance `Dialing` and `OutgoingConnectionError` translation during hole-punch |
| `cli/src/node/execute.rs` | Log external addresses when executing `AwaitHolePunch` |

## Non-Goals

- Full RFC 5780 NAT classification battery (address-restricted vs port-restricted distinction)
- Standalone `punchgate nat-test` CLI subcommand
- Automated fix selection based on NAT type (manual analysis from logs)
