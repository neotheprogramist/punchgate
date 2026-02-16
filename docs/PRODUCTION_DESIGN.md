# Punchgate: Architecture & Production Design Guide

A peer-to-peer NAT-traversing tunnel mesh built on libp2p. Every node runs identical code — roles (relay, service provider, consumer) emerge from network conditions.

---

## Table of Contents

- [Coalgebraic State Machine](#coalgebraic-state-machine)
- [libp2p Behaviour Composition](#libp2p-behaviour-composition)
- [Address Pipeline](#address-pipeline)
- [NAT Traversal & Hole Punching](#nat-traversal--hole-punching)
- [Tunnel System](#tunnel-system)
- [Service Discovery](#service-discovery)
- [Production Deployment](#production-deployment)
- [Testing Strategy](#testing-strategy)
- [Anti-Patterns & Lessons Learned](#anti-patterns--lessons-learned)

---

## Coalgebraic State Machine

The core of punchgate is a pure Mealy machine — a coalgebraic state machine where transitions are deterministic functions from `(State, Event)` to `(State, Vec<Command>)`.

### The MealyMachine Trait

```rust
pub trait MealyMachine: Sized {
    type Event;
    type Command;
    fn transition(self, event: Self::Event) -> (Self, Vec<Self::Command>);
}
```

This separation is the architectural keystone: the transition function **never touches the network**. It receives domain events, computes the next state, and emits commands as data. A separate executor interprets commands against the real swarm.

### State as Product of Orthogonal Coproducts

The peer state is a product (struct) of independent dimensions, each a coproduct (enum):

```
PeerState = Phase × NatStatus × RelayState × KnownPeers × ExternalAddrs
```

| Dimension      | Variants                                                          |
| -------------- | ----------------------------------------------------------------- |
| **Phase**      | `Initializing` → `Discovering` → `Participating` → `ShuttingDown` |
| **NatStatus**  | `Unknown` · `Public` · `Private`                                  |
| **RelayState** | `Idle` · `Requesting` · `Reserved`                                |

Each dimension evolves independently. An mDNS discovery event updates `known_peers` but never touches `Phase` or `NatStatus`. This orthogonality eliminates the combinatorial explosion — instead of `4 × 3 × 3 = 36` hand-written transitions, each dimension has focused, testable logic.

### Composed State Machines

Two sub-machines compose into a single `AppState`:

| Machine | Responsibility |
|---------|---------------|
| **PeerState** | Network lifecycle: connections, phases, NAT detection, relay negotiation |
| **TunnelState** | Tunnel lifecycle: DHT lookups, dialing, hole-punch gating, tunnel spawning |

**Composition logic**: Feed event to PeerState first. If phase changed, synthesize a `PhaseChanged` bridge event for TunnelState. Concatenate peer commands before tunnel commands (ordering matters: DHT operations must follow Kademlia bootstrap).

### Event Taxonomy (24 Variants)

Events are a closed enum hierarchy. Adding a variant forces exhaustive handling across the entire state machine — the compiler catches missing cases.

**Network lifecycle** (fed to PeerState):

| Event | Trigger |
|-------|---------|
| `ListeningOn` | New listen address bound |
| `BootstrapConnected` | Connection established with bootstrap peer |
| `ConnectionLost` | Peer disconnected (with remaining connection count) |
| `KademliaBootstrapOk/Failed` | DHT bootstrap result |
| `NatStatusChanged` | AutoNAT probe result |
| `RelayReservationAccepted/Failed` | Relay circuit reservation outcome |
| `MdnsDiscovered/Expired` | LAN peer discovery |
| `PeerIdentified` | Identify protocol exchanged |
| `ExternalAddrConfirmed/Expired` | External address lifecycle |
| `DiscoveryTimeout` | 30s safety net to force Participating |
| `NoBootstrapPeers` | No `--bootstrap` flag provided |
| `ShutdownRequested` | SIGTERM/Ctrl+C |

**Tunnel lifecycle** (fed to TunnelState):

| Event | Trigger |
|-------|---------|
| `DhtPeerLookupComplete` | Kademlia GetClosestPeers finished |
| `DhtServiceResolved/Failed` | Kademlia GetProviders result |
| `TunnelPeerConnected` | Connection to tunnel target (direct or relayed) |
| `HolePunchSucceeded/Failed` | DCUtR outcome |
| `PhaseChanged` | Bridge event from PeerState |

### Command Taxonomy (12 Variants)

Commands are declarative descriptions of side effects:

| Command | Executor Action |
|---------|----------------|
| `Dial(addr)` | `swarm.dial()` |
| `Listen(addr)` | `swarm.listen_on()` |
| `KademliaBootstrap` | `kademlia.bootstrap()` |
| `KademliaAddAddress` | `kademlia.add_address()` |
| `RequestRelayReservation` | `swarm.listen_on(relay_circuit_addr)` |
| `AddExternalAddress` | `swarm.add_external_address()` |
| `PublishServices` | `kademlia.start_providing()` + `put_record()` |
| `DhtLookupPeer` | `kademlia.get_closest_peers()` |
| `DhtGetProviders` | `kademlia.get_providers()` |
| `DialPeer` | `swarm.dial(peer_id)` |
| `SpawnTunnel` | `tokio::spawn(connect_tunnel(...))` |
| `Shutdown` | Event loop break |

### Event Loop Architecture

```
┌─────────────┐     ┌─────────────┐     ┌──────────────┐     ┌──────────┐
│ SwarmEvent   │ ──→ │ translate() │ ──→ │ transition() │ ──→ │ execute()│
│ (libp2p)     │     │ → Event     │     │ → (State,    │     │ → swarm  │
│              │     │             │     │    Commands)  │     │   calls  │
└─────────────┘     └─────────────┘     └──────────────┘     └──────────┘
      ↑                                                            │
      └────────────────────────────────────────────────────────────┘
```

The event loop (`node/mod.rs`) is a `tokio::select!` over:

1. **Swarm events** → translate → state transition → execute commands
2. **Discovery timeout** → force `Participating` after 30s
3. **Republish interval** → re-announce services every 5 minutes
4. **Bootstrap reconnect** → exponential backoff reconnection
5. **Shutdown signal** → graceful teardown

---

## libp2p Behaviour Composition

Nine sub-behaviours compose into a single `#[derive(NetworkBehaviour)]` struct:

| Behaviour | Protocol | Purpose |
|-----------|----------|---------|
| **Kademlia** | `/punchgate/kad/1.0.0` | DHT for peer routing and service discovery |
| **Identify** | `/punchgate/id/1.0.0` | Exchange peer metadata, observe external addresses |
| **AutoNAT** | `/libp2p/autonat/1.0.0` | Probe NAT reachability status |
| **Relay (server)** | `/libp2p/circuit/relay/0.2.0/hop` | Relay traffic for NAT'd peers |
| **Relay (client)** | `/libp2p/circuit/relay/0.2.0/stop` | Use relays when behind NAT |
| **DCUtR** | `/libp2p/dcutr` | Direct Connection Upgrade through Relay |
| **mDNS** *(opt)* | Multicast DNS | Zero-config LAN discovery |
| **Ping** | `/ipfs/ping/1.0.0` | Keepalive and RTT measurement |
| **Stream** | `/punchgate/tunnel/1.0.0` | Custom stream protocol for tunneling |

### SwarmBuilder Chain

```rust
SwarmBuilder::with_existing_identity(keypair)
    .with_tokio()
    .with_quic()                                              // QUIC-only transport
    .with_relay_client(noise::Config::new, yamux::Config::default)?
    .with_behaviour(|key, relay_client| Behaviour::new(key, relay_client))?
    .with_swarm_config(|cfg| cfg.with_idle_connection_timeout(Duration::from_secs(60)))
    .build()
```

QUIC is the sole transport. TCP was removed because QUIC provides built-in encryption (no separate noise handshake), multiplexing (no separate yamux), and better hole-punching characteristics (UDP-based, connection-ID demultiplexing).

---

## Address Pipeline

Understanding the address pipeline is critical for NAT traversal. There are **two separate address consumers** in libp2p, and conflating them is a common source of bugs.

### The Two Address Systems

| System | Purpose | Source | Storage |
|--------|---------|--------|---------|
| **Swarm confirmed set** | DHT/Kademlia advertisement ("where to find me") | `swarm.add_external_address()` | `confirmed_external_addr: HashSet` |
| **DCUtR candidate cache** | Hole-punch addresses ("where to dial me") | `FromSwarm::NewExternalAddrCandidate` | Internal LRU cache (max 20) |

These serve fundamentally different purposes:
- **DHT addresses** can be approximate — peers use them for initial routing, then connect via relay if direct fails
- **DCUtR addresses** must be **exact NAT mappings** — the remote peer dials these during the 3-attempt hole-punch window

### The Standard libp2p Address Flow

```
                                  ┌─────────────┐
                                  │   Identify   │  ← remote peer reports our
                                  │  (observed   │     NAT-mapped IP:port
                                  │   address)   │
                                  └──────┬───────┘
                                         │
                              ToSwarm::NewExternalAddrCandidate
                                         │
                                         ▼
                                  ┌─────────────┐
                                  │    Swarm     │  checks: is addr already
                                  │  (dispatch)  │  in confirmed set?
                                  └──────┬───────┘
                                         │
                           ┌─────────────┴─────────────┐
                           │ NO: dispatch to behaviours │
                           │ YES: SUPPRESSED            │
                           └─────────────┬─────────────┘
                                         │
                    FromSwarm::NewExternalAddrCandidate
                                         │
                  ┌──────────────────────┬┴──────────────────────┐
                  │                      │                       │
                  ▼                      ▼                       ▼
           ┌──────────┐          ┌──────────┐           ┌──────────────┐
           │  DCUtR   │          │ AutoNAT  │           │  Other       │
           │ (caches  │          │ (probes  │           │  behaviours  │
           │  in LRU) │          │  addr)   │           │              │
           └──────────┘          └─────┬────┘           └──────────────┘
                                       │
                            (on success, v2 only)
                              ToSwarm::ExternalAddrConfirmed
                                       │
                                       ▼
                                ┌─────────────┐
                                │    Swarm     │  add_external_address()
                                │ (confirms)   │  → suppresses future
                                └─────────────┘    candidates for this addr
```

### Critical Invariants

1. **DCUtR only listens for `NewExternalAddrCandidate`** — it ignores `ExternalAddrConfirmed`
2. **`add_external_address()` suppresses future candidates** — once confirmed, the same address will never reach DCUtR again
3. **Identify reports the NAT-observed address** — including the correct NAT-mapped port, which may differ from the local listen port
4. **DCUtR's `Candidates::add` filters relay circuit addresses internally** — no need to filter these at the application level for DCUtR

### Punchgate's Address Strategy

```rust
// At NewListenAddr: register self-computed address for DHT (approximate)
if let Some(ext_addr) = make_external_addr(address, external_ip) {
    swarm.add_external_address(ext_addr);  // for Kademlia only
}

// At NewExternalAddrCandidate: confirm Identify-observed addresses (exact)
if is_valid_external_candidate(address) {
    swarm.add_external_address(address);  // already in DCUtR's cache
}
```

The self-computed address uses `external_ip:local_port` which may have the wrong port (NAT remapping). This is acceptable for DHT because peers will connect via relay if direct fails. The real NAT-mapped address comes from Identify and flows to DCUtR through the standard pipeline.

### What NOT to Do

**Never manually inject addresses into DCUtR:**

```rust
// WRONG: self-computed address may have wrong NAT-mapped port
swarm.behaviour_mut().dcutr.on_swarm_event(
    FromSwarm::NewExternalAddrCandidate(NewExternalAddrCandidate { addr: &ext_addr }),
);
```

This wastes one of DCUtR's 3 hole-punch attempts on a potentially unreachable address. Let Identify handle it — Identify sees the real NAT mapping.

**Never confirm addresses before Identify runs:**

```rust
// WRONG: suppresses future Identify observations of the same address
swarm.add_external_address(external_ip_port.clone());
// If Identify later reports the same address, the candidate event is
// suppressed, and DCUtR never receives it.
```

If the self-computed address happens to match Identify's observation (port preserved by NAT), the Identify candidate is suppressed. If it doesn't match (port remapped), the Identify candidate flows normally. The risk is in the matching case — but DCUtR already has the self-computed address from the NewListenAddr handling, so this is acceptable. The key is to **never manually inject** into DCUtR.

---

## NAT Traversal & Hole Punching

### NAT Types and Hole-Punching Success

| NAT Type | Mapping | Filtering | Hole-Punch Success |
|----------|---------|-----------|-------------------|
| **Full cone** | Endpoint-independent | None | Always works |
| **Restricted cone** | Endpoint-independent | Address-restricted | Works if both sides dial simultaneously |
| **Port-restricted cone** | Endpoint-independent | Port+address-restricted | Works with precise timing |
| **Symmetric** | Endpoint-dependent | Port+address-restricted | Fails (each destination gets a different mapping) |

QUIC improves success rates over TCP because:
- UDP-based, so NAT mappings are for UDP flows (many NATs are more permissive with UDP)
- Connection-ID demultiplexing (not 4-tuple), allowing connections from unexpected source ports
- No TCP SYN/ACK state machine that firewalls track
- Measurements show ~81% QUIC success vs ~60% TCP in real-world deployments (source: [probe-lab/network-measurements](https://github.com/probe-lab/network-measurements))

### The DCUtR Protocol Flow

```
 Peer A (behind NAT)                Relay                Peer B (behind NAT)
        │                             │                         │
        │──── relay reservation ─────→│                         │
        │                             │                         │
        │                             │←── relay reservation ───│
        │                             │                         │
        │←── relayed connection ──────┼────── dial via relay ───│
        │                             │                         │
        │◄──────── CONNECT (A's addrs) ──────────────────────── │
        │──────── CONNECT (B's addrs) ────────────────────────► │
        │                             │                         │
        │◄──────── SYNC ──────────────┼──────────────────────── │
        │                             │                         │
        │═══════ simultaneous dial ═══╪═════════════════════════│
        │     A dials B's addr(s)     │    B dials A's addr(s)  │
        │                             │                         │
        │◄════════ direct connection (hole punched) ═══════════►│
```

**DCUtR attempts**: Each side gets 3 dial attempts. If the address list is polluted with unreachable addresses, all 3 may be wasted.

### Prerequisites for Successful Hole Punching

1. **Both peers must traverse their NAT** — connecting via LAN bypasses the NAT, creating no mapping
2. **Both peers need at least one connection through the NAT** — so Identify can observe the real NAT-mapped address
3. **The NAT type must permit inbound connections** — symmetric NATs create per-destination mappings that can't be predicted
4. **Port mappings must still be active** — UDP NAT mappings expire (typically 30-120s of inactivity)

### The Same-LAN Bootstrap Problem

If a peer and its bootstrap node are on the same LAN:

```
                     ┌──── LAN (10.0.0.0/24) ────┐
                     │                             │
              ┌──────┴──────┐              ┌───────┴──────┐
              │  Bootstrap  │              │   Peer B     │
              │ 10.0.0.1    │◄─────────────│ 10.0.0.2     │
              │ (public IP: │  LAN conn    │ (public IP:  │
              │  1.2.3.4)   │  NO NAT      │  5.6.7.8)    │
              └─────────────┘  traversal!   └──────────────┘

Identify on bootstrap sees Peer B at: 10.0.0.2:4001 (private!)
Peer B has NO NAT mapping.
External peer cannot reach 5.6.7.8:4001 — packet dropped by NAT.
```

**Solution**: Peer B should bootstrap via the public address (`1.2.3.4:4001`) even when on the same LAN. This forces traffic through the NAT, creating a mapping:

```bash
# Wrong (LAN, no NAT mapping):
--bootstrap /ip4/10.0.0.1/udp/4001/quic-v1/p2p/<ID>

# Correct (through NAT, creates mapping):
--bootstrap /ip4/1.2.3.4/udp/4001/quic-v1/p2p/<ID>
```

### Relay as Fallback

When hole-punching fails (symmetric NAT, no mapping, timeout), the relayed connection remains active. Tunnels can operate over the relay — higher latency but functional. The state machine tracks this via `TunnelState::awaiting_holepunch` → on failure, the tunnel is dropped (current behavior), but could be modified to fall back to relay.

---

## Tunnel System

### Wire Protocol

```
┌─────────────────┐  ┌─────────────────────┐  ┌───────────────────┐
│ Length (4 bytes  │  │ JSON Payload         │  │ Raw bidirectional │
│ big-endian u32)  │  │ (TunnelReq/Resp)    │  │ byte stream       │
└─────────────────┘  └─────────────────────┘  └───────────────────┘
        ▲                     ▲                        ▲
     framing              negotiation              data transfer
```

**Request**: `{ "service_name": "ssh" }`
**Response**: `{ "accepted": true }` or `{ "accepted": false, "reason": "unknown service" }`

After successful negotiation, raw `copy_bidirectional()` between the libp2p stream and the local TCP socket. Zero overhead after handshake.

### Server Side (Service Provider)

```
TCP service (e.g. SSH)     libp2p stream (from remote peer)
    localhost:22                    │
         ▲                         │
         │                         ▼
    ┌────┴────┐            ┌───────────────┐
    │ TCP     │◄═══════════│  Accept Loop  │
    │ connect │  copy_bidi │  (per stream) │
    └─────────┘            └───────────────┘
```

1. Accept incoming libp2p stream on `/punchgate/tunnel/1.0.0`
2. Read `TunnelRequest` → lookup service in exposed services map
3. `TcpStream::connect()` to local service
4. Write `TunnelResponse(accepted: true)`
5. `copy_bidirectional()` with compat layer (`futures::AsyncRead` ↔ `tokio::AsyncRead`)

### Client Side (Tunnel Consumer)

```
    Local TCP listener          libp2p stream (to remote peer)
     0.0.0.0:2222                      │
         │                             │
         ▼                             ▼
    ┌────────────┐            ┌────────────────┐
    │ TCP accept │═══════════►│  open_stream   │
    │ (per conn) │  copy_bidi │  (to provider) │
    └────────────┘            └────────────────┘
```

1. `TcpListener::bind(bind_addr)` — local listener
2. For each accepted TCP connection:
   - `control.open_stream(remote_peer, tunnel_protocol())`
   - Send `TunnelRequest`, read `TunnelResponse`
   - `copy_bidirectional()` until EOF

### Tunnel Lifecycle States

```
┌───────────────┐     ┌────────────────┐     ┌──────────────┐     ┌─────────┐
│ pending_by_   │ ──→ │ DHT lookup     │ ──→ │ dialing /    │ ──→ │ spawned │
│ peer/service  │     │ (GetProviders/ │     │ awaiting     │     │ (active │
│               │     │  GetClosest)   │     │ holepunch    │     │  task)  │
└───────────────┘     └────────────────┘     └──────────────┘     └─────────┘
                                                    │
                                              holepunch failed
                                                    │
                                                    ▼
                                             ┌──────────────┐
                                             │ dropped      │
                                             │ (peer in     │
                                             │  failed set) │
                                             └──────────────┘
```

---

## Service Discovery

### Publishing (Provider Side)

When entering `Participating` phase or after relay reservation:

1. For each `--expose name=host:port`:
   - `kademlia.start_providing(Key("/punchgate/svc/{name}"))` — register as provider
   - `kademlia.put_record(Key("/punchgate/svc/{name}"), json_metadata)` — store metadata

2. Republished every 5 minutes (Kademlia records expire)

### Discovery (Consumer Side)

When entering `Participating` with `--tunnel-by-name`:

1. `kademlia.get_providers(Key("/punchgate/svc/{name}"))` — query DHT
2. On `FoundProviders`: check if already connected to provider
   - **Connected (direct)**: spawn tunnel immediately
   - **Connected (relayed)**: wait for DCUtR hole-punch
   - **Disconnected**: dial provider, then spawn tunnel on connection

### Kademlia Key Design

```
/punchgate/svc/{service_name}
```

Service names are arbitrary strings. The same peer can expose multiple services. Multiple peers can expose the same service name (first provider returned wins).

---

## Production Deployment

### Bootstrap Node (Public IP)

```bash
podman run -d \
  --name punchgate-bootstrap \
  --publish 4001:4001/udp \        # MUST be UDP for QUIC
  -e PUNCHGATE_LISTEN="/ip4/0.0.0.0/udp/4001/quic-v1" \
  -v ./identity.key:/home/punchgate/identity.key \
  punchgate:latest
```

Key considerations:
- **Fixed port** (`4001`): predictable for firewall rules and peer configuration
- **UDP publish**: `--publish 4001:4001/udp` — podman/docker defaults to TCP
- **Persistent identity**: mount `identity.key` to preserve PeerId across restarts
- **External address**: auto-discovered via HTTP; override with `--external-address` if behind a load balancer

### Service Provider (Behind NAT)

```bash
cargo run --release -- \
  --bootstrap /ip4/<BOOTSTRAP_PUBLIC_IP>/udp/4001/quic-v1/p2p/<BOOTSTRAP_ID> \
  --expose ssh=127.0.0.1:22 \
  --expose http=127.0.0.1:8080
```

Key considerations:
- **Bootstrap via public IP**: even if on the same LAN as bootstrap, use the public address to create a NAT mapping
- **Relay is automatic**: on entering `Participating`, the node requests a relay reservation
- **Service publication**: advertised to DHT, discoverable by name
- **Multiple services**: comma-separated or multiple `--expose` flags

### Tunnel Consumer

```bash
# By peer ID (direct):
cargo run --release -- \
  --bootstrap /ip4/<BOOTSTRAP>/udp/4001/quic-v1/p2p/<ID> \
  --tunnel <PROVIDER_PEER_ID>:ssh@127.0.0.1:2222

# By service name (DHT discovery):
cargo run --release -- \
  --bootstrap /ip4/<BOOTSTRAP>/udp/4001/quic-v1/p2p/<ID> \
  --tunnel-by-name ssh@127.0.0.1:2222
```

### Container Networking

For QUIC in containers:

| Setting | Value | Why |
|---------|-------|-----|
| Port publish | `--publish 4001:4001/udp` | QUIC uses UDP, not TCP |
| External address | `--external-address <host_ip>` | Container can't auto-discover host IP |
| mDNS | `--no-default-features` | mDNS doesn't cross container networks |
| NET_ADMIN | Required for NAT simulation only | Production containers don't need it |

### Firewall Rules

```bash
# Bootstrap node: allow inbound QUIC
ufw allow 4001/udp

# Behind NAT: no inbound rules needed
# (relay + hole-punching handle connectivity)
```

---

## Testing Strategy

### Layer 1: Property-Based Unit Tests (88 tests)

All state machine transitions tested with random inputs via `proptest`. No hardcoded test values.

```rust
proptest! {
    #[test]
    fn shutdown_absorbs_any_phase(phase in arb_phase()) {
        let mut state = PeerState::new();
        state.phase = phase;
        let (new_state, _) = state.transition(Event::ShutdownRequested);
        prop_assert_eq!(new_state.phase, Phase::ShuttingDown);
    }
}
```

**Coverage**: every Event variant, every Phase transition, external address filtering, spec parsing roundtrips, backoff arithmetic.

### Layer 2: Integration Tests (5 tests)

Real libp2p swarms on localhost with QUIC transport:

| Test | Scenario |
|------|----------|
| `two_peer_mdns_discovery` | mDNS peer discovery on LAN |
| `three_peer_tunnel` | Full tunnel flow: connect → stream → echo |
| `service_discovery_tunnel` | DHT-based service discovery + tunnel |
| `bootstrap_disconnect_recovery` | Reconnect after bootstrap node failure |
| `outgoing_connection_error_after_peer_shutdown` | Error handling on stale connections |

### Layer 3: Local E2E Script

`scripts/e2e_tunnel_test.sh` — automated version of the manual 4-terminal test:

1. Build binary
2. Start Python echo server
3. Launch Peer B (expose echo)
4. Launch Peer A (tunnel to B)
5. Send payload via `nc`, verify echo
6. Save logs for analysis

### Layer 4: Container E2E with NAT Gateways

`scripts/e2e_compose_test.sh` — full NAT traversal in containers:

```
public (10.100.0.0/24):    bootstrap, nat-a, nat-b
lan-a  (10.0.1.0/24):      echo, workhorse (behind nat-a)
lan-b  (10.0.2.0/24):      client (behind nat-b), test-probe
```

NAT gateways use iptables for full-cone NAT simulation (SNAT + DNAT). Verifies:
- Tunnel data integrity through NAT
- NAT translation (workhorse sees client via NAT public IP)
- DCUtR events (informational — full-cone NAT allows direct connection)

### Test Pyramid

```
        ┌───────┐
        │  E2E  │  Container NAT topology (minutes)
        │compose│
       ─┴───────┴─
      ┌───────────┐
      │  E2E local│  Binary + echo server (seconds)
     ─┴───────────┴─
    ┌───────────────┐
    │  Integration  │  Real swarms, real DHT (seconds)
   ─┴───────────────┴─
  ┌───────────────────┐
  │  Property-based   │  Pure state machine (milliseconds)
  │  unit tests       │  88 tests, zero I/O
  └───────────────────┘
```

---

## Anti-Patterns & Lessons Learned

### 1. Never Manually Inject Addresses into DCUtR

**Wrong**:
```rust
swarm.behaviour_mut().dcutr.on_swarm_event(
    FromSwarm::NewExternalAddrCandidate(NewExternalAddrCandidate { addr: &my_addr }),
);
```

**Why**: Self-computed `external_ip:local_port` has the wrong port when NAT remaps. DCUtR only gets 3 dial attempts — wrong addresses waste them. Let Identify report the real NAT-mapped address.

### 2. Understand `add_external_address` Side Effects

`swarm.add_external_address(addr)` does two things:
1. Dispatches `ExternalAddrConfirmed` to all behaviours (DCUtR ignores this)
2. Adds to confirmed set, which **suppresses future `NewExternalAddrCandidate`** for the same address

This means early confirmation can block Identify observations from reaching DCUtR.

### 3. Confirm `NewExternalAddrCandidate` Selectively

**Wrong**: Confirm everything
```rust
SwarmEvent::NewExternalAddrCandidate { address } => {
    swarm.add_external_address(address);  // includes circuit addrs, private IPs, bare peer IDs
}
```

**Right**: Filter for valid public direct addresses
```rust
if is_valid_external_candidate(address) {  // !circuit && has_public_ip
    swarm.add_external_address(address);
}
```

### 4. Bootstrap via Public IP for NAT Mapping

If a peer is on the same LAN as its bootstrap node, it must still connect via the **public** address to create a NAT mapping. LAN connections bypass the NAT, making the peer unreachable from external networks.

### 5. UDP Port Publishing for Containers

`--publish 4001:4001` maps **TCP** only. QUIC requires `--publish 4001:4001/udp`. This is the most common deployment mistake after migrating from TCP to QUIC.

### 6. Relay Circuit Addresses Are Not Direct Addresses

Addresses containing `/p2p-circuit` are relay routes, not direct endpoints. They must be filtered from:
- External address candidates (for the confirmed set)
- External address rewriting (don't rewrite the IP in a circuit address)

DCUtR filters these internally, but the confirmed set does not.

### 7. AutoNAT v1 vs v2

Punchgate uses AutoNAT v1 (`autonat::Behaviour`), which:
- Reports `NatStatus::Public/Private/Unknown` via events
- Does **not** emit `ToSwarm::ExternalAddrConfirmed`
- Requires manual address confirmation

AutoNAT v2 (`autonat::v2::client::Behaviour`) automatically confirms addresses. Consider upgrading when rust-libp2p stabilizes v2.

### 8. Identify Address Translation

Identify performs address translation for outbound connections on ephemeral ports — it may replace the observed port with the listen port. This is important for understanding why Identify-reported addresses may differ from what the remote peer actually sees. The translation logic handles TCP and QUIC differently.

### 9. Connection-Gated Tunnels

Always check `swarm.is_connected(peer)` before `swarm.dial(peer)`. Kademlia's DHT walking establishes connections as a side-effect. If you dial an already-connected peer with `PeerCondition::Disconnected` (the default), the dial fails. Spawn tunnels directly over existing connections instead.

### 10. Exponential Backoff for Bootstrap Reconnection

Bootstrap disconnections trigger exponential backoff reconnection (1s → 2s → 4s → ... → 60s max). Each peer tracks its own backoff independently. Successful reconnection resets the backoff.

---

## References

- [libp2p DCUtR Specification](https://github.com/libp2p/specs/blob/master/relay/DCUtR.md)
- [libp2p Hole-Punching Specification](https://github.com/libp2p/specs/blob/master/connections/hole-punching.md)
- [libp2p AutoNAT Specification](https://github.com/libp2p/specs/blob/master/autonat/autonat-v1.md)
- [rust-libp2p DCUtR Implementation](https://github.com/libp2p/rust-libp2p/tree/master/protocols/dcutr)
- [rust-libp2p Hole-Punching Tutorial](https://docs.rs/libp2p/latest/libp2p/tutorials/hole_punching/index.html)
- [NAT Hole-Punching Measurements](https://github.com/probe-lab/network-measurements/blob/main/results/rfm15-nat-hole-punching.md)
- [QUIC Hole-Punching PR (rust-libp2p #3964)](https://github.com/libp2p/rust-libp2p/pull/3964)
