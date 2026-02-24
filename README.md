# Punchgate

Punchgate is a peer-to-peer NAT-traversing tunnel daemon built on libp2p.
Nodes can act as bootstrap, service provider, and client depending on config.

## Install

Prerequisites:

- Rust 1.85+
- macOS (`launchd`) or Linux (`systemd --user`) for service mode

Install from source with Cargo:

```bash
cargo install --path . --locked
```

Upgrade:

```bash
cargo install --path . --locked --force
```

Ensure Cargo bin directory is on `PATH`:

```bash
export PATH="$HOME/.cargo/bin:$PATH"
```

## Service UX (WireGuard-like)

Use service commands instead of long-running foreground terminals:

```bash
punchgate [--identity ... --listen ... --bootstrap ... --expose ... --tunnel ... --tunnel-by-name ...] up
punchgate status
punchgate logs
punchgate logs --follow
punchgate down
```

`punchgate up` writes runtime config and creates a per-user service:

- macOS: `~/Library/LaunchAgents/com.punchgate.daemon.plist`
- Linux: `~/.config/systemd/user/punchgate.service`
- Env file: `~/.config/punchgate/punchgate.env`

Service-mode configuration example (provider):

```bash
punchgate \
  --bootstrap /ip4/<BOOTSTRAP_IP>/udp/4001/quic-v1/p2p/<BOOTSTRAP_ID> \
  --expose workhorse-ssh=127.0.0.1:22 \
  up
```

Service-mode configuration example (client):

```bash
punchgate \
  --bootstrap /ip4/<BOOTSTRAP_IP>/udp/4001/quic-v1/p2p/<BOOTSTRAP_ID> \
  --tunnel-by-name workhorse-ssh@127.0.0.1:2222 \
  up
```

Linux keep-alive across logout:

```bash
sudo loginctl enable-linger "$USER"
```

## Minimal Topology

Bootstrap node (public):

```bash
punchgate --listen /ip4/0.0.0.0/udp/4001/quic-v1
```

Workhorse node (behind NAT, exposes SSH):

```bash
punchgate \
  --bootstrap /ip4/<BOOTSTRAP_IP>/udp/4001/quic-v1/p2p/<BOOTSTRAP_ID> \
  --expose ssh=127.0.0.1:22
```

Client node (tunnel by service name):

```bash
punchgate \
  --bootstrap /ip4/<BOOTSTRAP_IP>/udp/4001/quic-v1/p2p/<BOOTSTRAP_ID> \
  --tunnel-by-name ssh@127.0.0.1:2222
```

Verify:

```bash
ssh -p 2222 user@127.0.0.1
```

Punchgate prefers direct DCUtR paths, retries direct-upgrade attempts with
fresh address candidates when possible, and falls back to relay when retries
are exhausted.

## Logging

Punchgate uses `tracing` for all runtime/service output.

Examples:

```bash
RUST_LOG=info punchgate --listen /ip4/0.0.0.0/udp/0/quic-v1
RUST_LOG=debug punchgate logs --follow
```

Service logs:

- macOS: `~/Library/Logs/punchgate.log`, `~/Library/Logs/punchgate.err.log`
- Linux: `journalctl --user -u punchgate.service`

## Utility Script

The repository includes one operational helper script:

```bash
sudo ./scripts/create-user.sh --user <name> --ssh-key "<public-key>"
```

## Development

Build:

```bash
cargo build
```

Test:

```bash
cargo test
```

## License

MIT
