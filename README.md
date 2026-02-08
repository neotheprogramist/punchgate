# Punchgate

A peer-to-peer NAT-traversing tunnel mesh built on [libp2p](https://libp2p.io/). Every node runs identical code — roles (relay, service provider, consumer) emerge organically from network conditions.

## Core Principles

### 1. Single Unified Binary, No Fixed Roles

**Decision:** One binary, one codebase. No `bootstrap`, `serve`, `connect` subcommands.

**Justification:** Fixed roles create operational fragility — deploying a separate relay binary means a single point of failure and manual topology management. With a unified binary, every peer can relay for others, advertise services, and consume tunnels simultaneously. The network self-heals: if a relay goes down, another public peer fills the gap.

### 2. Coalgebraic State Machine (Pure Transitions)

**Decision:** The core state machine ([`state.rs`](cli/src/state.rs)) is a Mealy machine with a pure transition function:

```
(PeerState, Event) → (PeerState, Vec<Command>)
```

Events go in, a new state and a list of commands come out. Commands are data describing side effects (`Dial`, `KademliaBootstrap`, `PublishServices`, etc.) — they never touch the network directly. The event loop translates `SwarmEvent`s into `Event`s, feeds them to the state machine, then interprets the resulting `Command`s against the actual swarm.

**Justification:** Separating "what to do" from "how to do it" makes the state machine fully testable without a network. All 15 state machine tests run in microseconds with no I/O. This also enables [bisimulation verification](https://en.wikipedia.org/wiki/Bisimulation) — proving that two different event orderings produce equivalent outcomes (e.g., Kademlia-then-AutoNAT vs AutoNAT-then-Kademlia both reach `Participating`).

### 3. State as Product of Orthogonal Coproducts

**Decision:** The peer state is a product (struct) of independent dimensions, each a coproduct (enum):

| Dimension | Values |
|-----------|--------|
| **Phase** | `Initializing` → `Discovering` → `Participating` → `ShuttingDown` |
| **NatStatus** | `Unknown` \| `Public` \| `Private` |
| **RelayState** | `Idle` \| `Requesting` \| `Reserved` |

**Justification:** Each dimension evolves independently. An mDNS discovery event updates `known_peers` but never touches `Phase` or `NatStatus`. A NAT status change may trigger a relay reservation but never alters the Kademlia bootstrap state. This orthogonality eliminates a combinatorial explosion of states — instead of `4 × 3 × 3 = 36` hand-written transitions, each dimension has its own focused logic. Tests verify orthogonality explicitly (e.g., `mdns_does_not_affect_phase_or_nat`).

### 4. Commands as Data, Not Effects

**Decision:** The transition function never calls `swarm.dial()` or `swarm.listen_on()`. It emits `Command::Dial(addr)` or `Command::Listen(addr)`, and a separate `execute_commands()` function in `node.rs` interprets them.

**Justification:** This is the interpreter pattern. The state machine is a pure function that can be tested, replayed, and reasoned about algebraically. The executor is a thin translation layer that maps commands to libp2p API calls. If libp2p's API changes, only the executor changes — the state logic stays untouched.

### 5. Discovery Timeout as a Safety Net

**Decision:** After 30 seconds, the node force-transitions from `Discovering` to `Participating` even if Kademlia bootstrap or AutoNAT haven't completed.

**Justification:** On a LAN with no internet-reachable peers, AutoNAT may never resolve (there's no external server to probe). Without the timeout, the node would be stuck in `Discovering` forever. The timeout ensures the node becomes operational and starts accepting tunnels, trading completeness for liveness.

### 6. Automatic Relay Negotiation

**Decision:** When AutoNAT reports `Private` (behind NAT), the node automatically requests a relay reservation from the nearest known bootstrap peer. When NAT later becomes `Public`, the reservation is released.

**Justification:** Manual relay configuration is error-prone and doesn't adapt to changing network conditions (e.g., switching from WiFi to mobile data). Automatic negotiation means the node always has the best connectivity it can achieve.

### 7. Length-Prefixed JSON Wire Format for Tunnels

**Decision:** Tunnel negotiation uses a 4-byte big-endian length prefix followed by a JSON payload (`TunnelRequest`/`TunnelResponse`), then raw bidirectional byte copying.

**Justification:** JSON is human-debuggable and trivially extensible (add a field without breaking existing peers). The length prefix avoids delimiter ambiguity and enables bounded reads (max 64 KB, preventing memory exhaustion). Once negotiation completes, the tunnel is zero-overhead raw TCP bridging via `tokio::io::copy_bidirectional`.

### 8. futures-to-tokio Bridging via compat()

**Decision:** libp2p streams implement `futures::io::AsyncRead/Write`, but tokio's `copy_bidirectional` requires `tokio::io::AsyncRead/Write`. We bridge with `tokio_util::compat::FuturesAsyncReadCompatExt`.

**Justification:** This is the standard approach for the futures/tokio ecosystem split. The `.compat()` adapter is zero-cost (just trait delegation) and avoids reimplementing bidirectional copy.

## Architecture

```
cli/src/
├── bin/cli.rs       # CLI entry point (clap)
├── lib.rs           # Module declarations
├── protocol.rs      # Protocol IDs and constants
├── identity.rs      # Ed25519 keypair persistence
├── state.rs         # Pure coalgebraic state machine
├── behaviour.rs     # Unified NetworkBehaviour (9 sub-behaviours)
├── node.rs          # Swarm event loop (translate → transition → execute)
├── service.rs       # Service advertisement via Kademlia DHT
└── tunnel.rs        # Tunnel lifecycle (accept, connect, registry)
```

The nine sub-behaviours composed into a single `NetworkBehaviour`:

| Behaviour | Purpose |
|-----------|---------|
| **Kademlia** | Distributed hash table for peer and service discovery |
| **mDNS** | Zero-config LAN peer discovery |
| **Identify** | Exchange peer metadata on connection |
| **AutoNAT** | Determine NAT reachability status |
| **Relay (server)** | Act as relay for other NAT'd peers |
| **Relay (client)** | Use relays when behind NAT |
| **DCUtR** | Direct Connection Upgrade through Relay (hole punching) |
| **Ping** | Keepalive and latency measurement |
| **Stream** | Custom protocol streams for tunneling |

## Quick Start

### Prerequisites

- Rust 1.85+ (edition 2024)
- Two terminal windows

### Build

```bash
cargo build
```

### Run the Tests

```bash
# Unit tests (state machine, protocol, identity, service, tunnel parsing)
cargo test -p cli --lib

# Integration tests (mDNS discovery + three-peer tunnel)
cargo test -p cli --test integration
```

### E2E Test: Tunnel an Echo Server Between Two Peers

This test runs two punchgate peers on your local machine. Peer B exposes a local TCP echo server, and you connect to it through Peer A's tunnel. You'll need four terminal windows.

**Terminal 1 — Start the echo server:**

```bash
python3 scripts/echo_server.py --port 7777
```

You should see the echo server logging that it's listening. Leave it running.

**Terminal 2 — Start Peer B, exposing the echo server:**

```bash
cargo run -p cli -- \
  --identity /tmp/peer-b.key \
  --listen /ip4/127.0.0.1/tcp/0 \
  --expose echo=127.0.0.1:7777
```

Note the peer ID and listening address printed by Peer B, e.g.:
```
INFO cli::identity: generated new identity peer_id=12D3KooW...
INFO cli::node:     listening on /ip4/127.0.0.1/tcp/54321
```

**Terminal 3 — Start Peer A, tunnel to Peer B's echo service:**

Replace `<PEER_B_ID>` and `<PEER_B_ADDR>` with the values from Terminal 2:

```bash
cargo run -p cli -- \
  --identity /tmp/peer-a.key \
  --listen /ip4/127.0.0.1/tcp/0 \
  --bootstrap <PEER_B_ADDR>/p2p/<PEER_B_ID> \
  --tunnel <PEER_B_ID>:echo@127.0.0.1:9000
```

**Terminal 4 — Test the tunnel:**

```bash
echo "hello punchgate" | nc 127.0.0.1 9000
```

You should see `hello punchgate` echoed back. The echo server in Terminal 1 will log the bytes it received and sent. The data traveled:
```
nc → TCP:9000 → Peer A → libp2p stream → Peer B → TCP:7777 → echo server → back
```

**Cleanup:** Ctrl+C each terminal.

### Scripted E2E Test

The same flow as above, fully automated in a single command:

```bash
./scripts/e2e_tunnel_test.sh
```

The script builds the binary, starts the echo server, launches both peers, waits for readiness via log polling, sends the test payload, verifies the echo, and cleans up. On failure it preserves logs for debugging.

### Automated Integration Test

The tunnel flow is also tested programmatically at the library level:

```bash
cargo test -p cli --test integration -- three_peer_tunnel
```

This test:
1. Spawns three libp2p swarms (A, B, C) with TCP on random ports
2. Starts a TCP echo server
3. B registers to accept tunnel streams and exposes the echo service
4. C connects to B and opens a tunnel
5. Sends `"hello punchgate!"` through the tunnel and verifies the echo

## Configuration

| Flag | Default | Description |
|------|---------|-------------|
| `--identity PATH` | `identity.key` | Persistent Ed25519 keypair file |
| `--listen MULTIADDR` | `/ip4/0.0.0.0/tcp/0` | Address(es) to listen on |
| `--bootstrap MULTIADDR` | *(none)* | Peer(s) to dial on startup |
| `--expose NAME=HOST:PORT` | *(none)* | Expose a local TCP service to the mesh |
| `--tunnel PEER:SVC@BIND` | *(none)* | Tunnel a remote service to a local port |

Environment: `PUNCHGATE_IDENTITY` overrides `--identity`. `RUST_LOG` controls log verbosity (default: `info`).

## License

MIT
