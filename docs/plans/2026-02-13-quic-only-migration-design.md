# QUIC-Only Migration + DCUtR Enhancement

## Problem

TCP DCUtR hole punching fails with symmetric NAT (random port mappings per connection). Both real-world deployment nodes (bycur workhorse, client) sit behind symmetric NATs, making TCP simultaneous open impossible. QUIC uses a fundamentally different mechanism — UDP packet flooding — that works with most symmetric NATs.

Additionally, the compose E2E test runs on a flat bridge network where peers connect directly via DHT, so DCUtR never fires. This gives no coverage of the relay + hole punch flow.

## Decisions

- **QUIC only** — drop TCP entirely from the SwarmBuilder
- **No relay fallback** — HolePunchFailed drops tunnel specs permanently
- **Network isolation in compose** — separate bridge networks force relay+DCUtR

## Design

### 1. Transport: QUIC Only

SwarmBuilder chain:

```
with_existing_identity → with_tokio → with_quic → with_relay_client → with_behaviour
```

Remove `.with_tcp(Default::default(), noise::Config::new, yamux::Config::default)`. QUIC has built-in encryption (TLS 1.3) and multiplexing, so Noise and Yamux are not needed for the main transport. The relay client still uses Noise+Yamux over relay circuits (protocol-level, not transport-level).

CLI default listen address changes from `/ip4/0.0.0.0/tcp/0` to `/ip4/0.0.0.0/udp/0/quic-v1`.

### 2. Integration Tests

All `build_test_swarm` functions switch from TCP to QUIC:

- Remove TCP transport from SwarmBuilder
- Listen on `/ip4/127.0.0.1/udp/0/quic-v1`
- Remove `noise` and `yamux` imports

### 3. E2E Scripts

Both scripts update addresses and log grep patterns:

- `--listen /ip4/127.0.0.1/udp/0/quic-v1`
- Grep for `listening on /ip4/127.0.0.1/udp/` instead of `tcp`

### 4. Compose: Network Isolation

Three bridge networks:

| Network | Subnet | Purpose |
|---------|--------|---------|
| public | 172.28.1.0/24 | Bootstrap's reachable network |
| private-a | 172.28.2.0/24 | Workhorse's isolated network |
| private-b | 172.28.3.0/24 | Client's isolated network |

Container assignments:

| Container | Networks | IPs |
|-----------|----------|-----|
| echo | private-a (172.28.2.20) | Reachable only by workhorse |
| bootstrap | public (172.28.1.10), private-a (172.28.2.10), private-b (172.28.3.10) | Bridge between all networks |
| workhorse | private-a (172.28.2.30) | Reaches bootstrap via 172.28.2.10 |
| client | private-b (172.28.3.40) | Reaches bootstrap via 172.28.3.10 |

Workhorse and client are on separate networks — they can only communicate through bootstrap as a relay. DCUtR fires because the initial connection is relayed.

All listen addresses: `/ip4/0.0.0.0/udp/4001/quic-v1`.

### 5. DCUtR: QUIC Hole Punching

No code changes to DCUtR behaviour — `libp2p-quic v0.13.0` already implements `Transport::dial_as_listener` for QUIC hole punching. Switching to QUIC-only means DCUtR automatically uses the QUIC path:

1. Initiator (A) dials responder's address immediately on receiving Sync
2. Responder (B) sends random UDP packets at 10-200ms intervals to create NAT mapping
3. A's QUIC handshake succeeds through the mapping B created

This is asymmetric (unlike TCP simultaneous open) and works with most symmetric NATs.

### 6. External Address Rewriting

Already handles UDP multiaddrs — the `make_external_addr` function rewrites the IP component regardless of protocol. Existing test `rewrites_private_ip_in_quic_addr` covers this.

### 7. Files to Modify

| File | Change |
|------|--------|
| `cli/src/node/mod.rs` | Remove `.with_tcp()`, remove `noise`/`yamux` imports |
| `cli/src/bin/cli.rs` | Default listen → QUIC |
| `cli/tests/integration.rs` | QUIC transport, QUIC addresses |
| `scripts/e2e_tunnel_test.sh` | QUIC addresses, log pattern updates |
| `scripts/e2e_compose_test.sh` | QUIC addresses, log pattern updates |
| `compose.yaml` | Three networks, QUIC addresses, per-network bootstrap addrs |
| `README.md` | Update example commands |
