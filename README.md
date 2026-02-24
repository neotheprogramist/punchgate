# Punchgate

A peer-to-peer NAT-traversing tunnel mesh built on [libp2p](https://libp2p.io/). Every node runs identical code — roles (relay, service provider, consumer) emerge organically from network conditions.

> For design decisions and architectural details, see [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

## Quick Start

### Prerequisites

- Rust 1.85+ (edition 2024)
- Two terminal windows (foreground mode)

### Build

```bash
# Default build (DHT-only discovery)
cargo build

# With mDNS for LAN discovery
cargo build --features mdns

# With AutoNAT for dynamic NAT detection
cargo build --features autonat
```

### Background Service Mode (macOS, Ubuntu, Fedora)

Punchgate can run in the background with a WireGuard-like UX:

```bash
# Install/update and start background service
punchgate up [--identity ... --listen ... --bootstrap ... --expose ... --tunnel ... --tunnel-by-name ...]

# Show service status
punchgate status

# Show service logs
punchgate logs
punchgate logs --follow

# Stop and disable service
punchgate down
```

If you are running from source without installing the binary, use:
`cargo run -p cli -- <command>` (for example, `cargo run -p cli -- up`).

What `punchgate up` does:

1. Writes runtime env config to `~/.config/punchgate/punchgate.env`
2. Generates per-user service unit files:
   `~/Library/LaunchAgents/com.punchgate.daemon.plist` (macOS) or
   `~/.config/systemd/user/punchgate.service` (Linux)
3. Starts/enables the service (`launchd` on macOS, `systemd --user` on Linux)

Reference templates are also checked into the repo:

- `deploy/launchd/com.punchgate.daemon.plist`
- `deploy/systemd/punchgate.service`

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
  --listen /ip4/127.0.0.1/udp/0/quic-v1 \
  --expose echo=127.0.0.1:7777
```

Note the peer ID and listening address printed by Peer B, e.g.:

```
INFO cli::identity: generated new identity peer_id=12D3KooW...
INFO cli::node:     listening on /ip4/127.0.0.1/udp/54321/quic-v1
```

**Terminal 3 — Start Peer A, tunnel to Peer B's echo service:**

Replace `<PEER_B_ID>` and `<PEER_B_ADDR>` with the values from Terminal 2:

```bash
cargo run -p cli -- \
  --identity /tmp/peer-a.key \
  --listen /ip4/127.0.0.1/udp/0/quic-v1 \
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

The full NAT traversal flow connects a client to a service behind NAT via a public relay node, then upgrades to a direct connection via DCUtR hole punching. This requires three nodes on separate networks.

**Node 1 — Bootstrap (public IP, acts as relay):**

```bash
cargo run -p cli -- \
  --listen /ip4/0.0.0.0/udp/4001/quic-v1
```

Note the peer ID from the log output (e.g. `local_peer_id=12D3KooW...`). All other nodes need this to connect.

**Node 2 — Workhorse (behind NAT, exposes SSH):**

```bash
cargo run -p cli -- \
  --bootstrap /ip4/<BOOTSTRAP_IP>/udp/4001/quic-v1/p2p/<BOOTSTRAP_ID> \
  --expose ssh=127.0.0.1:22
```

Any peer with `--bootstrap` is automatically treated as behind NAT: it requests a relay circuit and publishes the `ssh` service to the DHT. The relay is used only to coordinate DCUtR; tunnel activation is deferred until a direct (non-relayed) connection is established.

**Node 3 — Client (tunnel to workhorse's SSH by name):**

```bash
cargo run -p cli -- \
  --bootstrap /ip4/<BOOTSTRAP_IP>/udp/4001/quic-v1/p2p/<BOOTSTRAP_ID> \
  --tunnel-by-name ssh@127.0.0.1:2222
```

The client discovers the `ssh` provider via the Kademlia DHT — no need to know the workhorse's peer ID.

**Verify:**

```bash
ssh -p 2222 user@127.0.0.1
```

The data path: `ssh → TCP:2222 → Client → DCUtR direct path → Workhorse → TCP:22 → sshd`.

## Production Deployment

### Build the Image

```bash
podman build -t punchgate:local .
```

The resulting image is Alpine-based and runs as a non-root `punchgate` user. The default build has no optional features — mDNS and AutoNAT are opt-in via `--build-arg FEATURES="--features mdns,autonat"` if needed.

### Bootstrap Node (Public VPS)

The bootstrap node needs a public IP with UDP port 4001 open. It acts as a Kademlia DHT entry point and relay for NATed peers.

Create the env file:

```bash
# .env
PUNCHGATE_LISTEN=/ip4/0.0.0.0/udp/4001/quic-v1
RUST_LOG=cli=info
```

Start the container with the QUIC port published:

```bash
podman run --detach \
  --env-file .env \
  --name punchgate \
  --publish 0.0.0.0:4001:4001/udp \
  localhost/punchgate:local
```

Get the peer ID from the logs:

```bash
podman logs punchgate
# look for: local_peer_id=12D3KooW...
```

All other nodes need this peer ID and the bootstrap's public IP to join the mesh.

### Non-Bootstrap Peer (Behind NAT)

Peers behind NAT do not publish any ports. They connect outbound to bootstrap and reserve relay circuits for coordination, but tunnel traffic is permitted only on direct DCUtR-upgraded connections.

Create the env file:

```bash
# .env
PUNCHGATE_LISTEN=/ip4/0.0.0.0/udp/4001/quic-v1
PUNCHGATE_BOOTSTRAP=/ip4/<BOOTSTRAP_PUBLIC_IP>/udp/4001/quic-v1/p2p/<BOOTSTRAP_PEER_ID>
RUST_LOG=cli=info

# Workhorse: expose a local service to the mesh
PUNCHGATE_EXPOSE=myapp=host.containers.internal:8080

# Client: tunnel a remote service to a local port
# (use one of these, not both)
PUNCHGATE_TUNNEL_BY_NAME=myapp@0.0.0.0:2222
```

Start without port publishing:

```bash
podman run --detach \
  --env-file .env \
  --name punchgate \
  localhost/punchgate:local
```

Check the logs:

```bash
podman logs punchgate
```

### NAT Traversal Behavior

NAT status is derived from topology: any peer with `--bootstrap` is treated as private (behind NAT), while a peer with no bootstrap addresses is public (the bootstrap node itself). No runtime detection is needed.

The connection upgrade path:

1. **Relay** — NATed peers reserve a relay circuit through bootstrap on connect
2. **DCUtR hole punch** — both peers attempt simultaneous QUIC connections to punch through their NATs
3. **Direct-preferred tunnel activation** — tunnels prefer direct non-relayed connectivity; if hole-punch retries are exhausted, tunnel setup falls back to the relayed path

For dynamic NAT detection, enable the `autonat` feature at build time.

### Environment Variables

All CLI flags have corresponding environment variables:

| Variable                   | Equivalent flag    | Description                            |
| -------------------------- | ------------------ | -------------------------------------- |
| `PUNCHGATE_IDENTITY`       | `--identity`       | Path to Ed25519 keypair file           |
| `PUNCHGATE_LISTEN`         | `--listen`         | Multiaddr(s) to listen on              |
| `PUNCHGATE_BOOTSTRAP`      | `--bootstrap`      | Bootstrap peer multiaddr(s)            |
| `PUNCHGATE_EXPOSE`         | `--expose`         | Expose local service: `name=host:port` |
| `PUNCHGATE_TUNNEL`         | `--tunnel`         | Tunnel by peer ID: `peer:svc@bind`     |
| `PUNCHGATE_TUNNEL_BY_NAME` | `--tunnel-by-name` | Tunnel by DHT lookup: `svc@bind`       |
| `RUST_LOG`                 | —                  | Log verbosity (default: `info`)        |

### Container E2E Test: NAT Traversal via Compose

The full relay → DCUtR hole-punch → direct tunnel flow can be tested locally with simulated NAT gateways. Requires `podman compose`.

```bash
./scripts/e2e_compose_test.sh
```

The topology uses three isolated networks with full-cone NAT gateways:

```
┌─────────────────────────────────────────────────────────────────┐
│                    public (10.100.0.0/24)                       │
│                                                                 │
│   bootstrap          nat-a              nat-b                   │
│   10.100.0.10        10.100.0.11        10.100.0.12             │
└───────┬───────────────┬──────────────────┬──────────────────────┘
        │               │                  │
        │     ┌─────────┴─────────┐  ┌─────┴──────────────┐
        │     │  lan-a (internal) │  │  lan-b (internal)   │
        │     │  10.0.1.0/24      │  │  10.0.2.0/24        │
        │     │                   │  │                     │
        │     │  echo    workhrs  │  │  client  test-probe │
        │     │  .20       .30    │  │  .40       .50      │
        │     └───────────────────┘  └─────────────────────┘
```

The test verifies: relay reservation, DCUtR hole-punch success, direct tunnel establishment, and end-to-end echo payload roundtrip through both NATs.

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
  --listen /ip4/0.0.0.0/udp/0/quic-v1 \
  2> logs/my-peer.log
```

For clean capture, use either `cargo run --release 2>&1 | tee run.log` or just `cargo run --release | tee run.log`.

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

| Flag                        | Default                      | Description                                         |
| --------------------------- | ---------------------------- | --------------------------------------------------- |
| `--identity PATH`           | `identity.key`               | Persistent Ed25519 keypair file                     |
| `--listen MULTIADDR`        | `/ip4/0.0.0.0/udp/0/quic-v1` | Address(es) to listen on                            |
| `--bootstrap MULTIADDR`     | _(none)_                     | Peer(s) to dial on startup                          |
| `--expose NAME=HOST:PORT`   | _(none)_                     | Expose a local TCP service to the mesh              |
| `--tunnel PEER:SVC@BIND`    | _(none)_                     | Tunnel a remote service to a local port             |
| `--tunnel-by-name SVC@BIND` | _(none)_                     | Tunnel by service name (discovers provider via DHT) |

All flags accept corresponding `PUNCHGATE_*` environment variables (see [Environment Variables](#environment-variables)). `RUST_LOG` controls log verbosity (default: `info`).

## License

MIT
