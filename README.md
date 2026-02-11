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

**Justification:** Separating "what to do" from "how to do it" makes the state machine fully testable without a network. All 18 state machine tests run in microseconds with no I/O. This also enables [bisimulation verification](https://en.wikipedia.org/wiki/Bisimulation) — proving that two different event orderings produce equivalent outcomes (e.g., Kademlia-then-AutoNAT vs AutoNAT-then-Kademlia both reach `Participating`).

### 3. State as Product of Orthogonal Coproducts

**Decision:** The peer state is a product (struct) of independent dimensions, each a coproduct (enum):

| Dimension      | Values                                                            |
| -------------- | ----------------------------------------------------------------- |
| **Phase**      | `Initializing` → `Discovering` → `Participating` → `ShuttingDown` |
| **NatStatus**  | `Unknown` \| `Public` \| `Private`                                |
| **RelayState** | `Idle` \| `Requesting` \| `Reserved`                              |

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
├── behaviour.rs     # Unified NetworkBehaviour (8–9 sub-behaviours)
├── node.rs          # Swarm event loop (translate → transition → execute)
├── service.rs       # Service advertisement via Kademlia DHT
└── tunnel.rs        # Tunnel lifecycle (accept, connect, registry)

scripts/
├── echo_server.py         # TCP echo server for testing
├── e2e_tunnel_test.sh     # Automated E2E test — direct LAN (saves logs to logs/)
├── e2e_compose_test.sh    # Automated E2E test — NAT traversal via containers
├── log_parse.py           # Shared structured log parser
├── log_summary.py         # Quick one-page peer overview
├── log_connections.py     # Connection topology analysis
├── log_latency.py         # Ping RTT statistics
├── log_tunnels.py         # Tunnel throughput metrics
└── log_phases.py          # State machine lifecycle analysis

Containerfile              # Multi-stage build (cargo-chef + sccache)
compose.yaml               # 4-service topology for NAT traversal E2E test
logs/                      # Peer logs (auto-populated by e2e test, gitignored)
```

The sub-behaviours composed into a single `NetworkBehaviour` (mDNS is compile-time optional via the `mdns` feature flag):

| Behaviour          | Purpose                                                           |
| ------------------ | ----------------------------------------------------------------- |
| **Kademlia**       | Distributed hash table for peer and service discovery             |
| **mDNS** _(opt)_   | Zero-config LAN peer discovery (`--features mdns`, on by default) |
| **Identify**       | Exchange peer metadata on connection                              |
| **AutoNAT**        | Determine NAT reachability status                                 |
| **Relay (server)** | Act as relay for other NAT'd peers                                |
| **Relay (client)** | Use relays when behind NAT                                        |
| **DCUtR**          | Direct Connection Upgrade through Relay (hole punching)           |
| **Ping**           | Keepalive and latency measurement                                 |
| **Stream**         | Custom protocol streams for tunneling                             |

## Quick Start

### Prerequisites

- Rust 1.85+ (edition 2024)
- Two terminal windows

### Build

```bash
# Default build (includes mDNS for LAN discovery)
cargo build

# Without mDNS (forces DHT-only discovery — used in container E2E tests)
cargo build --no-default-features
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

The script builds the binary, starts the echo server, launches both peers, waits for readiness via log polling, sends the test payload, verifies the echo, and cleans up. Logs are always saved to `logs/` for post-hoc analysis (see [Observability](#observability) below). On failure it also preserves raw logs in the temp directory.

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

### NAT Traversal: Tunnel Through a Relay

The full NAT traversal flow connects a client to a service behind NAT via a public relay node, then upgrades to a direct connection via DCUtR hole punching. This requires three nodes on separate networks (or Docker containers / network namespaces).

**Node 1 — Bootstrap (public IP, acts as relay):**

```bash
cargo run -p cli -- \
  --listen /ip4/0.0.0.0/tcp/4001
```

**Node 2 — Workhorse (behind NAT, exposes SSH):**

```bash
cargo run -p cli -- \
  --bootstrap /ip4/<BOOTSTRAP_IP>/tcp/4001/p2p/<BOOTSTRAP_ID> \
  --nat-status private \
  --expose ssh=127.0.0.1:22
```

The `--nat-status private` flag bypasses the AutoNAT wait, immediately triggering relay reservation. The workhorse connects to the bootstrap node, gets a relay circuit reservation, advertises the relay circuit address as an external address, and publishes services to the DHT.

**Node 3 — Client (tunnel to workhorse's SSH):**

```bash
cargo run -p cli -- \
  --bootstrap /ip4/<BOOTSTRAP_IP>/tcp/4001/p2p/<BOOTSTRAP_ID> \
  --tunnel <WORKHORSE_ID>:ssh@127.0.0.1:2222
```

The client enters `Participating`, performs a Kademlia DHT lookup for the workhorse's peer ID, discovers the relay circuit address, dials through the relay, and DCUtR auto-triggers hole punching. On success, the tunnel spawns over the direct connection.

**Verify:**

```bash
ssh -p 2222 user@127.0.0.1
```

The data path: `ssh → TCP:2222 → Client → [DCUtR direct or relay] → Workhorse → TCP:22 → sshd`.

### Container E2E Test: NAT Traversal via Compose

The full relay → DCUtR → tunnel flow can be tested locally using containers. The test script builds the image with `--no-default-features` (mDNS disabled), forcing all discovery through the Kademlia DHT — same as a real multi-network deployment.

Requires `podman compose`.

```bash
./scripts/e2e_compose_test.sh
```

The script orchestrates four containers on a single bridge network (`172.28.0.0/24`):

| Container     | IP          | Role                                              |
| ------------- | ----------- | ------------------------------------------------- |
| **echo**      | 172.28.0.20 | TCP echo server (python:3-alpine)                 |
| **bootstrap** | 172.28.0.10 | Public relay node                                 |
| **workhorse** | 172.28.0.30 | NAT'd peer (`--nat-status private`), exposes echo |
| **client**    | 172.28.0.40 | Tunnels to workhorse's echo, port 2222 on host    |

Startup is phased: bootstrap starts first (peer ID parsed from logs), then workhorse (connects to bootstrap, gets relay reservation), then client (DHT lookup → relay dial → DCUtR → tunnel). The test sends a payload through `nc → 127.0.0.1:2222` and verifies the echo.

To build the image separately:

```bash
# Production image (mDNS enabled — default)
podman build -t punchgate:local .

# E2E test image (no mDNS — forces DHT-only discovery)
podman build -t punchgate:local --build-arg FEATURES=--no-default-features .
```

## Observability

Punchgate emits structured `tracing` events at every observation point — connections, pings, phase transitions, and tunnel lifecycle. Python scripts in `scripts/` parse these logs and produce aggregate reports.

### Collecting Logs

**Automated (recommended):** Run the E2E test — logs are saved to `logs/` automatically:

```bash
./scripts/e2e_tunnel_test.sh
# Logs saved: logs/peer-a.log, logs/peer-b.log, logs/echo.log
```

**Manual:** Redirect stderr to a file (tracing writes to stderr):

```bash
cargo run -p cli -- \
  --identity peer.key \
  --listen /ip4/0.0.0.0/tcp/0 \
  2> logs/my-peer.log
```

Use `RUST_LOG` to control verbosity:

| Level                | What you get                                                      |
| -------------------- | ----------------------------------------------------------------- |
| `RUST_LOG=info`      | Connections, pings, phase transitions, tunnel events (default)    |
| `RUST_LOG=cli=debug` | Adds per-event state machine processing (used by `log_phases.py`) |
| `RUST_LOG=debug`     | Full libp2p internals (very verbose)                              |

### Analysis Scripts

All scripts auto-detect logs in `logs/`, accept an explicit file path, or read from stdin. Reports are automatically saved to `logs/` alongside the raw logs (e.g. `logs/summary.txt`, `logs/latency.csv`):

```bash
# Default: reads all *.log files from logs/, saves report to logs/summary.txt
python scripts/log_summary.py

# Explicit file
python scripts/log_summary.py logs/peer-a.log

# Pipe from a running node
cargo run -p cli -- ... 2>&1 | python scripts/log_summary.py

# Directory argument
python scripts/log_summary.py logs/
```

Typical workflow after an E2E test:

```bash
./scripts/e2e_tunnel_test.sh        # populates logs/*.log
python scripts/log_summary.py       # → logs/summary.txt
python scripts/log_connections.py   # → logs/connections.txt
python scripts/log_latency.py       # → logs/latency.txt
python scripts/log_tunnels.py       # → logs/tunnels.txt
python scripts/log_phases.py        # → logs/phases.txt
```

#### `log_summary.py` — Quick Overview

One-page summary of a peer's activity: peer ID, listen addresses, phase progression, unique peers seen, ping stats, tunnel throughput, and warning/error counts.

```
============================================================
PUNCHGATE PEER SUMMARY
============================================================
  Peer ID: 12D3KooW...
  Log span: 10:30:00 - 10:35:42 (148 entries)

  Phase progression:
    10:30:01 Initializing -> Discovering (BootstrapConnected)
    10:30:04 Discovering -> Participating (NatStatusChanged)

  Unique peers seen: 3
  Ping measurements: 42 (failures: 1)
  RTT mean: 12.3ms
  Tunnel sessions: 5 accepted, 0 rejected, 0 errors
  Total bytes transferred: 1.2MB
  Warnings: 1
  Errors:   0
```

#### `log_connections.py` — Connection Topology

Tracks peer connection/disconnection events. Shows peak connection count, churn rate, per-peer session durations, and a timeline of opens/closes. Useful for diagnosing connectivity instability.

```bash
python scripts/log_connections.py           # text table
python scripts/log_connections.py --json    # machine-readable
```

#### `log_latency.py` — Ping RTT Analysis

Per-peer RTT statistics (min, mean, median, p95, p99, max) and failure rates. Flags unhealthy peers (>10% failure rate or >500ms mean RTT).

```bash
python scripts/log_latency.py              # text table
python scripts/log_latency.py --csv        # CSV for graphing
```

#### `log_tunnels.py` — Tunnel Throughput

Aggregate and per-service tunnel metrics: accepted/rejected sessions, bytes transferred in each direction, error rates. Splits server-side and client-side views.

```bash
python scripts/log_tunnels.py              # text table
python scripts/log_tunnels.py --json       # machine-readable
```

#### `log_phases.py` — State Machine Lifecycle

Phase transition timeline, time spent in each phase, event frequency distribution, and anomaly detection (regressions like `Participating → Discovering`).

```bash
python scripts/log_phases.py               # text report
python scripts/log_phases.py --json        # machine-readable
```

### Structured Events Reference

| Event                                    | Level | Key Fields                                                     | Source    |
| ---------------------------------------- | ----- | -------------------------------------------------------------- | --------- |
| `connection opened`                      | INFO  | `peer`, `endpoint`                                             | node.rs   |
| `connection closed`                      | INFO  | `peer`, `remaining`                                            | node.rs   |
| `ping`                                   | INFO  | `peer`, `rtt`                                                  | node.rs   |
| `ping failed`                            | WARN  | `peer`, `error`                                                | node.rs   |
| `phase transition`                       | INFO  | `event`, `from`, `to`, `commands`                              | node.rs   |
| `event processed`                        | DEBUG | `event`, `commands`                                            | node.rs   |
| `starting DHT lookup for tunnel target`  | INFO  | `target_peer`                                                  | node.rs   |
| `dialing tunnel target after DHT lookup` | INFO  | `target_peer`                                                  | node.rs   |
| `connected to tunnel target via relay`   | INFO  | `peer_id`                                                      | node.rs   |
| `hole punch succeeded, spawning tunnel`  | INFO  | `remote_peer`                                                  | node.rs   |
| `hole punch failed — tunnel cannot...`   | ERROR | `remote_peer`, `reason`                                        | node.rs   |
| `tunnel failed`                          | ERROR | `error`                                                        | node.rs   |
| `tunnel request`                         | INFO  | `peer_id`, `service`                                           | tunnel.rs |
| `tunnel accepted`                        | INFO  | `peer_id`, `service`, `local_addr`                             | tunnel.rs |
| `tunnel rejected`                        | WARN  | `peer_id`, `service`, `reason`                                 | tunnel.rs |
| `tunnel closed`                          | INFO  | `peer_id`, `service`, `bytes_to_service`, `bytes_from_service` | tunnel.rs |
| `tunnel error`                           | WARN  | `peer_id`, `error`                                             | tunnel.rs |
| `client tunnel closed`                   | INFO  | `service`, `bytes_to_remote`, `bytes_from_remote`              | tunnel.rs |
| `client tunnel rejected`                 | WARN  | `service`, `reason`                                            | tunnel.rs |
| `client tunnel error`                    | WARN  | `service`, `error`                                             | tunnel.rs |

## Configuration

| Flag                      | Default              | Description                                   |
| ------------------------- | -------------------- | --------------------------------------------- |
| `--identity PATH`         | `identity.key`       | Persistent Ed25519 keypair file               |
| `--listen MULTIADDR`      | `/ip4/0.0.0.0/tcp/0` | Address(es) to listen on                      |
| `--bootstrap MULTIADDR`   | _(none)_             | Peer(s) to dial on startup                    |
| `--expose NAME=HOST:PORT` | _(none)_             | Expose a local TCP service to the mesh        |
| `--tunnel PEER:SVC@BIND`  | _(none)_             | Tunnel a remote service to a local port       |
| `--nat-status STATUS`     | _(none)_             | Override NAT detection: `private` or `public` |

Environment: `PUNCHGATE_IDENTITY` overrides `--identity`. `RUST_LOG` controls log verbosity (default: `info`).

## License

MIT
