# Punchgate

A peer-to-peer NAT-traversing tunnel mesh built on [libp2p](https://libp2p.io/). Every node runs identical code — roles (relay, service provider, consumer) emerge organically from network conditions.

> For design decisions and architectural details, see [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

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

You should see `hello punchgate` echoed back. The data traveled:

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
  --expose ssh=127.0.0.1:22
```

Relay reservation is automatic. The workhorse connects to the bootstrap node, immediately requests a relay circuit reservation, advertises the relay circuit address as an external address, and publishes services to the DHT. When AutoNAT later confirms the node is publicly reachable, the relay is released.

**Node 3 — Client (tunnel to workhorse's SSH):**

```bash
cargo run -p cli -- \
  --bootstrap /ip4/<BOOTSTRAP_IP>/tcp/4001/p2p/<BOOTSTRAP_ID> \
  --tunnel <WORKHORSE_ID>:ssh@127.0.0.1:2222
```

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

| Container     | IP          | Role                                           |
| ------------- | ----------- | ---------------------------------------------- |
| **echo**      | 172.28.0.20 | TCP echo server (python:3-alpine)              |
| **bootstrap** | 172.28.0.10 | Public relay node                              |
| **workhorse** | 172.28.0.30 | NAT'd peer, exposes echo via relay             |
| **client**    | 172.28.0.40 | Tunnels to workhorse's echo, port 2222 on host |

To build the image separately:

```bash
# Production image (mDNS enabled — default)
podman build -t punchgate:local .

# E2E test image (no mDNS — forces DHT-only discovery)
podman build -t punchgate:local --build-arg FEATURES=--no-default-features .
```

## Observability

Punchgate emits structured `tracing` events at every observation point — connections, pings, phase transitions, and tunnel lifecycle. Python scripts in `scripts/` parse these logs and produce aggregate reports. See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md#structured-events-reference) for the full events reference.

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

All scripts auto-detect logs in `logs/`, accept an explicit file path, or read from stdin. Reports are automatically saved to `logs/` alongside the raw logs:

```bash
./scripts/e2e_tunnel_test.sh        # populates logs/*.log
python scripts/log_summary.py       # → logs/summary.txt
python scripts/log_connections.py   # → logs/connections.txt
python scripts/log_latency.py       # → logs/latency.txt
python scripts/log_tunnels.py       # → logs/tunnels.txt
python scripts/log_phases.py        # → logs/phases.txt
```

| Script               | Output                                                            |
| -------------------- | ----------------------------------------------------------------- |
| `log_summary.py`     | One-page overview: peer ID, phases, ping stats, tunnel throughput |
| `log_connections.py` | Connection topology, churn rate, per-peer session durations       |
| `log_latency.py`     | Per-peer RTT statistics (min, mean, p95, p99, max), failure rates |
| `log_tunnels.py`     | Tunnel metrics: accepted/rejected sessions, bytes transferred     |
| `log_phases.py`      | Phase transition timeline, time per phase, anomaly detection      |

## Configuration

| Flag                      | Default              | Description                             |
| ------------------------- | -------------------- | --------------------------------------- |
| `--identity PATH`         | `identity.key`       | Persistent Ed25519 keypair file         |
| `--listen MULTIADDR`      | `/ip4/0.0.0.0/tcp/0` | Address(es) to listen on                |
| `--bootstrap MULTIADDR`   | _(none)_             | Peer(s) to dial on startup              |
| `--expose NAME=HOST:PORT` | _(none)_             | Expose a local TCP service to the mesh  |
| `--tunnel PEER:SVC@BIND`  | _(none)_             | Tunnel a remote service to a local port |

Environment: `PUNCHGATE_IDENTITY` overrides `--identity`. `RUST_LOG` controls log verbosity (default: `info`).

## License

MIT
