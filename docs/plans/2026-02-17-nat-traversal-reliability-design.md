# NAT Traversal Reliability Fixes

Four targeted fixes to improve NAT traversal reliability and reduce time-to-tunnel.

## Problem

Production logs show consistent patterns:
- ~10s wasted dialing unreachable private/loopback addresses during DHT walks
- DCUtR hole-punch fails on first attempt (NAT pinholes not established), succeeds on second
- Relay circuits expire after 120s (libp2p default), killing active tunnels
- `holepunch_failed` flag not set on timeout, causing inconsistent retry behavior

## Fix 1: Filter Private/Loopback from Kademlia

**Problem**: `PeerIdentified` pushes ALL `listen_addrs` to Kademlia, including `127.0.0.1` and `172.16.x.x` from container peers. Kademlia dials these during DHT walks, each timing out after ~5s.

**Solution**: In `PeerState::transition` for `PeerIdentified`, only emit `KademliaAddAddress` for addresses with public IPs (via `external_addr::has_public_ip()`). Continue storing all addresses in `known_peers` — they're needed for relay addr construction on bootstrap peers.

**Files**: `cli/src/state/peer.rs`

## Fix 2: Increase Relay max_circuit_duration

**Problem**: `relay::Behaviour::new(peer_id, Default::default())` uses the libp2p default `max_circuit_duration` of 120 seconds. Active tunnels over relay are killed after 2 minutes.

**Solution**: Add `MAX_CIRCUIT_DURATION = 3600s` constant to `protocol.rs`. Construct `relay::Config` with this value in `Behaviour::new`. Matches existing `IDLE_CONNECTION_TIMEOUT`.

**Files**: `cli/src/protocol.rs`, `cli/src/behaviour.rs`

## Fix 3: Periodic Hole-Punch Retry

**Problem**: After DCUtR fails and tunnel falls back to relay, no mechanism retries hole-punching. With fix 2 extending circuit duration to 1 hour, the natural retry (circuit reset triggers new DCUtR) won't happen for a long time.

**Solution**: Periodic direct-dial retry mechanism:
- New constant: `HOLEPUNCH_RETRY_INTERVAL = 60s`
- New event: `HolePunchRetryTick { peer }`
- New map in event loop: `holepunch_retries: HashMap<PeerId, Instant>`
- After `HolePunchTimeout` spawns relay tunnel, schedule retry at `now + 60s`
- On tick: state machine checks if peer still in `relayed_peers`, emits `DialPeer` for direct attempt
- Success: `TunnelPeerConnected { relayed: false }` clears relayed state; retries stop
- Failure: reschedule for another 60s
- Stop on: peer disconnect, direct connection success, or shutdown

Existing tunnel unaffected — `open_stream` uses whatever connection exists.

**Files**: `cli/src/state/event.rs`, `cli/src/state/command.rs`, `cli/src/state/tunnel.rs`, `cli/src/node/mod.rs`, `cli/src/protocol.rs`

## Fix 4: Set holepunch_failed on Timeout

**Problem**: `HolePunchTimeout` handler spawns relay tunnel but doesn't set `holepunch_failed`. If a new tunnel spec arrives for the same peer while connected via relay, `route_connected_peer` waits for hole punch instead of immediately falling back.

**Solution**: Add `self.holepunch_failed.insert(peer)` to `HolePunchTimeout` handler, mirroring `HolePunchFailed`.

**Files**: `cli/src/state/tunnel.rs`

## Testing

- Update existing property tests for address filtering behavior
- Add property tests for: holepunch retry tick, holepunch_failed consistency on timeout
- Existing integration tests unaffected (use mDNS, not Kademlia filtering)
