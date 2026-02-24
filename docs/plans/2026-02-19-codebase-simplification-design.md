# Codebase Simplification: Surgical Removal of NAT Priming

**Date**: 2026-02-19
**Status**: Approved

## Goal

Remove NAT priming infrastructure and enable AutoNAT by default. DCUtR becomes the sole hole-punching strategy. STUN mapping detection and port mapping remain for diagnostics and UPnP fallback.

## Context

The NAT priming experiment (sending packets from the QUIC socket to create NAT mappings before DCUtR) was added to help restricted-filtering NATs. In production, the priming dial established connections via shared LAN/VPN addresses, bypassing NAT entirely. For cross-NAT scenarios (the primary deployment target), DCUtR's simultaneous-open protocol is the correct mechanism. Priming added 3 commands, 2 events, and ~200 lines of state tracking that complicate the codebase without solving the cross-NAT problem.

AutoNAT is documented as a core behaviour but is currently disabled by default behind a feature flag. Enabling it replaces a synthetic `NatStatus::Private` injection hack with real NAT reachability probing.

## Removals

### Commands (3)
- `PrimeNatMapping { peer, peer_addrs }` — priming dial before DCUtR
- `PrimeAndDialDirect { peer, peer_addrs }` — retry priming + direct dial
- `AwaitHolePunch { peer }` — no-op diagnostic log

### Events (2)
- `HolePunchRetryTick { peer }` — 60s retry timer for priming
- `NatMappingDetected(NatMapping)` — STUN result fed into state machine (now checked directly at startup)

### TunnelState Fields (2)
- `relayed_peers: HashSet<PeerId>` — tracked relayed peers for priming decisions
- `peer_external_addrs: HashMap<PeerId, Vec<Multiaddr>>` — stored addresses for priming targets

### Files (1)
- `cli/src/nat_probe_protocol.rs` — filtering probe wire types

### nat_probe.rs Removals (~100 lines)
- `NatFiltering` enum
- `NatClassification` struct
- `probe_filtering()` function
- All filtering-related code and tests

### node/mod.rs Removals
- `holepunch_retry_timers` HashMap + tokio::select! arm
- Synthetic `NatStatus::Private` injection at startup
- Filtering probe spawning/handling logic
- NAT probe protocol stream handler

### protocol.rs Removals
- `NAT_PROBE_PROTOCOL` constant
- `HOLEPUNCH_RETRY_INTERVAL` constant

## Changes

### Enable AutoNAT by Default
`cli/Cargo.toml`:
```toml
[features]
default = ["autonat"]
```

Removes the synthetic NatStatus hack. AutoNAT probes real reachability through bootstrap peers.

### Reduce Hole-Punch Timeout
`HOLEPUNCH_TIMEOUT`: 30s → 15s

DCUtR's 3 attempts take ~15s. Faster relay fallback improves user experience.

### Simplify nat_probe.rs
Keep only STUN mapping detection:
- `NatMapping` enum (`EndpointIndependent`, `AddressDependent`, `Unknown`)
- `detect_nat_mapping()` — probes 2 STUN servers, classifies port behavior
- `is_holepunch_viable()` — returns false for symmetric NAT

### TunnelState Retention
- `nat_mapping: NatMapping` — **kept**. Set once at startup, gates whether hole-punch is attempted for symmetric NATs.

## What Stays

- DCUtR + hole-punch timeout (15s) + relay fallback
- STUN EIM detection (skip doomed hole-punches on symmetric NAT)
- Port mapping (`port_mapping.rs`) — UPnP/NAT-PMP fallback
- `external_addr.rs` — address validation
- `relayed_connections` tracking in node/mod.rs (feeds `TunnelPeerConnected { relayed }`)
- `is_relayed()` / `has_circuit()` in translate.rs
- `HolePunchTimeout` event (the 15s relay-fallback timer)
- Full state machine architecture (MealyMachine, PeerState, TunnelState, AppState)

## Follow-Up (Out of Scope)

Investigate why DCUtR's simultaneous-open fails for cross-NAT peers with EIM:
1. Address list pollution (LAN IPs in DCUtR candidates)
2. Port remapping mismatch in Identify observations
3. Candidate suppression via early `add_external_address()`
4. SYNC timing issues with restricted-filtering NATs

## Estimated Impact

- ~300-400 lines removed
- 5-10 lines changed
- 3 commands, 2 events, 1 file removed from codebase
- AutoNAT enabled, providing real NAT detection
