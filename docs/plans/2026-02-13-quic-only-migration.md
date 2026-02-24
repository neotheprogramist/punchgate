# QUIC-Only Migration Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Drop TCP transport in favor of QUIC-only, enabling QUIC DCUtR hole punching for symmetric NAT traversal.

**Architecture:** Remove TCP from SwarmBuilder, change all addresses to QUIC (`/udp/PORT/quic-v1`), restructure compose with network isolation to force relay+DCUtR in E2E tests.

**Tech Stack:** libp2p 0.56.0, libp2p-quic 0.13.0 (QUIC hole punching built-in), podman compose

---

### Task 1: Remove TCP features from workspace Cargo.toml

**Files:**
- Modify: `Cargo.toml:13-28`

**Step 1: Remove TCP, noise, yamux features from libp2p dependency**

Remove `"noise"`, `"tcp"`, `"yamux"` from the libp2p features list. These are TCP-specific — QUIC uses built-in TLS 1.3 and multiplexing. The relay client still needs noise+yamux but gets them through `"relay"` feature.

```toml
libp2p = { version = "0.56.0", features = [
    "autonat",
    "dcutr",
    "dns",
    "identify",
    "kad",
    "macros",
    "noise",
    "ping",
    "quic",
    "relay",
    "serde",
    "tokio",
] }
```

Note: `"noise"` stays because `with_relay_client(noise::Config::new, yamux::Config::default)` still needs it for relay circuit encryption. `"yamux"` is pulled in by `"relay"` transitively, no need to list explicitly. `"tcp"` is dropped entirely.

**Step 2: Verify it compiles**

Run: `cargo check -p cli 2>&1 | tail -5`
Expected: Compilation errors in `node/mod.rs` and `integration.rs` (they still reference TCP). That's expected — we fix them in Tasks 2-3.

---

### Task 2: Switch node SwarmBuilder to QUIC-only

**Files:**
- Modify: `cli/src/node/mod.rs:14,68-83`

**Step 1: Update imports**

Replace line 14:
```rust
use libp2p::{Multiaddr, PeerId, SwarmBuilder, noise, swarm::SwarmEvent, yamux};
```
with:
```rust
use libp2p::{Multiaddr, PeerId, SwarmBuilder, noise, swarm::SwarmEvent, yamux};
```

Actually — `noise` and `yamux` are still needed for `with_relay_client`. The imports stay the same.

**Step 2: Remove `.with_tcp()` from SwarmBuilder chain**

Replace lines 68-83:
```rust
    let mut swarm = SwarmBuilder::with_existing_identity(keypair)
        .with_tokio()
        .with_tcp(
            Default::default(),
            noise::Config::new,
            yamux::Config::default,
        )?
        .with_quic()
        .with_relay_client(noise::Config::new, yamux::Config::default)?
        .with_behaviour(
            |key, relay_client| -> Result<Behaviour, Box<dyn std::error::Error + Send + Sync>> {
                Behaviour::new(key, relay_client).map_err(|e| e.to_string().into())
            },
        )?
        .with_swarm_config(|cfg| cfg.with_idle_connection_timeout(IDLE_CONNECTION_TIMEOUT))
        .build();
```

with:
```rust
    let mut swarm = SwarmBuilder::with_existing_identity(keypair)
        .with_tokio()
        .with_quic()
        .with_relay_client(noise::Config::new, yamux::Config::default)?
        .with_behaviour(
            |key, relay_client| -> Result<Behaviour, Box<dyn std::error::Error + Send + Sync>> {
                Behaviour::new(key, relay_client).map_err(|e| e.to_string().into())
            },
        )?
        .with_swarm_config(|cfg| cfg.with_idle_connection_timeout(IDLE_CONNECTION_TIMEOUT))
        .build();
```

Key change: remove the entire `.with_tcp(...)?.with_quic()` block and replace with just `.with_quic()`.

**Step 3: Verify it compiles**

Run: `cargo check -p cli --lib 2>&1 | tail -5`
Expected: Success (library code compiles). Integration tests still broken.

---

### Task 3: Change CLI default listen address to QUIC

**Files:**
- Modify: `cli/src/bin/cli.rs:21-26`

**Step 1: Update default listen address**

Replace:
```rust
    #[arg(
        long,
        default_value = "/ip4/0.0.0.0/tcp/0",
        env = "PUNCHGATE_LISTEN",
        value_delimiter = ','
    )]
    listen: Vec<Multiaddr>,
```

with:
```rust
    #[arg(
        long,
        default_value = "/ip4/0.0.0.0/udp/0/quic-v1",
        env = "PUNCHGATE_LISTEN",
        value_delimiter = ','
    )]
    listen: Vec<Multiaddr>,
```

**Step 2: Verify binary compiles**

Run: `cargo check -p cli 2>&1 | tail -5`
Expected: Success for the binary. Integration tests still reference TCP.

---

### Task 4: Migrate integration tests to QUIC

**Files:**
- Modify: `cli/tests/integration.rs`

**Step 1: Update imports**

Replace lines 7-9:
```rust
use libp2p::{
    Multiaddr, PeerId, SwarmBuilder, identify, identity::Keypair, kad, noise, swarm::SwarmEvent,
    yamux,
};
```

with:
```rust
use libp2p::{
    Multiaddr, PeerId, SwarmBuilder, identify, identity::Keypair, kad, swarm::SwarmEvent,
};
```

Remove `noise` and `yamux` — test swarms don't use relay client, so no need for these.

**Step 2: Rewrite `build_test_swarm` to use QUIC**

Replace the `build_test_swarm` function (lines 18-71):
```rust
fn build_test_swarm() -> (libp2p::Swarm<TestBehaviour>, PeerId, libp2p_stream::Control) {
    let keypair = libp2p::identity::Keypair::generate_ed25519();
    let peer_id = PeerId::from(keypair.public());

    let mut swarm = SwarmBuilder::with_existing_identity(keypair.clone())
        .with_tokio()
        .with_quic()
        .with_behaviour(|key| {
            let peer_id = PeerId::from(key.public());

            let kad_config = kad::Config::new(cli::protocol::kad_protocol());
            let store = kad::store::MemoryStore::new(peer_id);
            let mut kademlia = kad::Behaviour::with_config(peer_id, store, kad_config);
            kademlia.set_mode(Some(kad::Mode::Server));

            #[cfg(feature = "mdns")]
            let mdns = mdns::tokio::Behaviour::new(mdns::Config::default(), peer_id)
                .expect("create mDNS behaviour");

            let identify = identify::Behaviour::new(identify::Config::new(
                cli::protocol::IDENTIFY_PROTOCOL.to_string(),
                key.public(),
            ));

            let stream = libp2p_stream::Behaviour::new();

            TestBehaviour {
                kademlia,
                #[cfg(feature = "mdns")]
                mdns,
                identify,
                stream,
            }
        })
        .expect("build swarm behaviour")
        .with_swarm_config(|cfg| cfg.with_idle_connection_timeout(Duration::from_secs(30)))
        .build();

    let control = swarm.behaviour().stream.new_control();
    swarm
        .listen_on(
            "/ip4/127.0.0.1/udp/0/quic-v1"
                .parse()
                .expect("parse listen multiaddr"),
        )
        .expect("start listening");

    (swarm, peer_id, control)
}
```

Key changes:
- Remove `.with_tcp(...)` + `.expect("build TCP transport")`
- Use `.with_quic()` (infallible, no `?` or `.expect()`)
- Listen address: `/ip4/127.0.0.1/udp/0/quic-v1`
- The `.with_behaviour` closure takes just `|key|` (no relay client)

**Step 3: Rewrite `build_test_swarm_with_keypair` the same way**

Same changes as `build_test_swarm` but taking `keypair: Keypair` parameter:
```rust
fn build_test_swarm_with_keypair(
    keypair: Keypair,
) -> (libp2p::Swarm<TestBehaviour>, PeerId, libp2p_stream::Control) {
    let peer_id = PeerId::from(keypair.public());

    let mut swarm = SwarmBuilder::with_existing_identity(keypair.clone())
        .with_tokio()
        .with_quic()
        .with_behaviour(|key| {
            // ... same body as build_test_swarm ...
        })
        .expect("build swarm behaviour")
        .with_swarm_config(|cfg| cfg.with_idle_connection_timeout(Duration::from_secs(30)))
        .build();

    let control = swarm.behaviour().stream.new_control();
    swarm
        .listen_on(
            "/ip4/127.0.0.1/udp/0/quic-v1"
                .parse()
                .expect("parse listen multiaddr"),
        )
        .expect("start listening");

    (swarm, peer_id, control)
}
```

**Step 4: Update address parsing in `wait_for_listen_addr`**

No change needed — it just waits for any `NewListenAddr` event, which now emits QUIC addresses.

**Step 5: Run unit tests**

Run: `cargo test -p cli --lib 2>&1 | tail -10`
Expected: All 84 tests pass

**Step 6: Run integration tests**

Run: `cargo test -p cli --test integration 2>&1 | tail -20`
Expected: All 5 integration tests pass

**Step 7: Run clippy**

Run: `cargo clippy -p cli -- -D warnings 2>&1 | tail -5`
Expected: No warnings

---

### Task 5: Migrate E2E tunnel test script to QUIC

**Files:**
- Modify: `scripts/e2e_tunnel_test.sh`

**Step 1: Update listen addresses**

Replace all `--listen /ip4/127.0.0.1/tcp/0` with `--listen /ip4/127.0.0.1/udp/0/quic-v1` (two occurrences: peer B at line 165, peer A at line 195).

**Step 2: Update log grep patterns**

Replace `listening on /ip4/127.0.0.1/tcp/` with `listening on /ip4/127.0.0.1/udp/` (three occurrences: lines 170, 177, 201).

**Step 3: Update address parsing regex**

Replace the PEER_B_ADDR extraction (line 177):
```bash
PEER_B_ADDR=$(grep -o 'listening on /ip4/127.0.0.1/tcp/[0-9]*' "$PEER_B_LOG_CLEAN" | head -1 | sed 's/listening on //')
```

with:
```bash
PEER_B_ADDR=$(grep -o 'listening on /ip4/127.0.0.1/udp/[0-9]*/quic-v1' "$PEER_B_LOG_CLEAN" | head -1 | sed 's/listening on //')
```

**Step 4: Run E2E tunnel test**

Run: `./scripts/e2e_tunnel_test.sh`
Expected: PASS — echo through tunnel matches

---

### Task 6: Migrate compose to QUIC + network isolation

**Files:**
- Modify: `compose.yaml`

**Step 1: Rewrite compose.yaml with three networks**

```yaml
networks:
  public:
    driver: bridge
    ipam:
      config:
        - subnet: 172.28.1.0/24
  private-a:
    driver: bridge
    ipam:
      config:
        - subnet: 172.28.2.0/24
  private-b:
    driver: bridge
    ipam:
      config:
        - subnet: 172.28.3.0/24

services:
  echo:
    image: python:3-alpine
    networks:
      private-a:
        ipv4_address: 172.28.2.20
    volumes:
      - ./scripts/echo_server.py:/echo_server.py:ro
    command: python3 /echo_server.py --host 0.0.0.0 --port 7777

  bootstrap:
    build:
      context: .
      args:
        FEATURES: "--no-default-features"
    networks:
      public:
        ipv4_address: 172.28.1.10
      private-a:
        ipv4_address: 172.28.2.10
      private-b:
        ipv4_address: 172.28.3.10
    environment:
      PUNCHGATE_LISTEN: "/ip4/0.0.0.0/udp/4001/quic-v1"

  workhorse:
    build:
      context: .
      args:
        FEATURES: "--no-default-features"
    depends_on:
      - echo
      - bootstrap
    networks:
      private-a:
        ipv4_address: 172.28.2.30
    environment:
      PUNCHGATE_LISTEN: "/ip4/0.0.0.0/udp/4001/quic-v1"
      PUNCHGATE_BOOTSTRAP: "/ip4/172.28.2.10/udp/4001/quic-v1/p2p/${BOOTSTRAP_ID}"
      PUNCHGATE_EXPOSE: "echo=172.28.2.20:7777"

  client:
    build:
      context: .
      args:
        FEATURES: "--no-default-features"
    depends_on:
      - workhorse
    networks:
      private-b:
        ipv4_address: 172.28.3.40
    ports:
      - "2222:2222"
    environment:
      PUNCHGATE_LISTEN: "/ip4/0.0.0.0/udp/4001/quic-v1"
      PUNCHGATE_BOOTSTRAP: "/ip4/172.28.3.10/udp/4001/quic-v1/p2p/${BOOTSTRAP_ID}"
      PUNCHGATE_TUNNEL_BY_NAME: "echo@0.0.0.0:2222"
```

Key changes from old compose:
- Three networks instead of one — workhorse and client cannot reach each other
- Bootstrap on all three networks (bridge)
- QUIC addresses everywhere (`/udp/4001/quic-v1`)
- Workhorse bootstraps via `172.28.2.10` (private-a view of bootstrap)
- Client bootstraps via `172.28.3.10` (private-b view of bootstrap)

---

### Task 7: Migrate E2E compose test script to QUIC

**Files:**
- Modify: `scripts/e2e_compose_test.sh`

**Step 1: Update log wait patterns**

Replace `listening on /ip4/` with `listening on /ip4/` (this pattern still works for QUIC, but the bootstrap wait on line 111 should match UDP):

Line 111: `wait_for_container_log bootstrap "listening on /ip4/" "bootstrap listening"` — this still works, QUIC listener logs include `/ip4/`.

Line 125: same for workhorse — still works.

Line 143: same for client — still works.

**Step 2: Update the hole punch verification section**

The DCUtR verification at lines 183-200 now expects hole punch events because network isolation forces relay. Update the final section to expect DCUtR success on isolated networks:

Replace the `no DCUtR events` else-branch warning (lines 196-199):
```bash
    warn "no DCUtR events — peers connected directly (expected on flat bridge network)"
```

This case should no longer happen with network isolation. Change to a failure:
```bash
    fail "no DCUtR events — with network isolation, relay+DCUtR should have fired"
```

**Step 3: Run E2E compose test**

Run: `./scripts/e2e_compose_test.sh`
Expected: PASS — tunnel works, DCUtR hole punch succeeds (or fails but tunnel still works via relay connection that established before the punch attempt). The key assertion is that DCUtR **fires** — on isolated networks, the connection to workhorse must go through relay.

Note: On a flat bridge-like container network, QUIC DCUtR may or may not actually succeed (since there's no real NAT to punch through, just network isolation). The important thing is that the relay path works and DCUtR attempts happen. If DCUtR fails on container networks (no real symmetric NAT), the test should still pass on the tunnel assertion. Adjust the hole punch section to warn-not-fail on DCUtR failure in containers.

---

### Task 8: Update README with QUIC addresses

**Files:**
- Modify: `README.md`

**Step 1: Update all example commands**

Replace all TCP multiaddrs with QUIC equivalents:

| Old | New |
|-----|-----|
| `/ip4/0.0.0.0/tcp/0` | `/ip4/0.0.0.0/udp/0/quic-v1` |
| `/ip4/0.0.0.0/tcp/4001` | `/ip4/0.0.0.0/udp/4001/quic-v1` |
| `/ip4/127.0.0.1/tcp/0` | `/ip4/127.0.0.1/udp/0/quic-v1` |
| `/ip4/<BOOTSTRAP_IP>/tcp/4001/p2p/<ID>` | `/ip4/<BOOTSTRAP_IP>/udp/4001/quic-v1/p2p/<ID>` |

Update the configuration table default from `/ip4/0.0.0.0/tcp/0` to `/ip4/0.0.0.0/udp/0/quic-v1`.

Update the listening log example from `listening on /ip4/127.0.0.1/tcp/54321` to `listening on /ip4/127.0.0.1/udp/54321/quic-v1`.

**Step 2: Verify no stale TCP references remain**

Run: `grep -rn 'tcp' README.md`
Expected: No TCP multiaddrs remain (the word "tcp" may appear in prose like "TCP echo server" which is fine).

---

### Task 9: Final verification

**Step 1: Run all unit tests**

Run: `cargo test -p cli --lib`
Expected: All tests pass

**Step 2: Run integration tests**

Run: `cargo test -p cli --test integration`
Expected: All tests pass

**Step 3: Run clippy**

Run: `cargo clippy -p cli -- -D warnings`
Expected: No warnings

**Step 4: Run E2E tunnel test**

Run: `./scripts/e2e_tunnel_test.sh`
Expected: PASS

**Step 5: Run E2E compose test**

Run: `./scripts/e2e_compose_test.sh`
Expected: PASS with DCUtR events logged

**Step 6: Grep for stale TCP references**

Run: `grep -rn '/tcp/' cli/src/ scripts/ compose.yaml README.md`
Expected: No TCP multiaddr references in any source/script/config file
