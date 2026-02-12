# Architecture

A peer-to-peer NAT-traversing tunnel mesh built on [libp2p](https://libp2p.io/). Every node runs identical code — roles (relay, service provider, consumer) emerge organically from network conditions.

## Core Principles

### 1. Single Unified Binary, No Fixed Roles

**Decision:** One binary, one codebase. No `bootstrap`, `serve`, `connect` subcommands.

**Justification:** Fixed roles create operational fragility — deploying a separate relay binary means a single point of failure and manual topology management. With a unified binary, every peer can relay for others, advertise services, and consume tunnels simultaneously. The network self-heals: if a relay goes down, another public peer fills the gap.

### 2. Coalgebraic State Machine (Pure Transitions)

**Decision:** The core state machine ([`cli/src/state.rs`](../cli/src/state.rs)) is a Mealy machine with a pure transition function:

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

### 6. Reactive Relay Negotiation (Private-First)

**Decision:** On entering `Participating`, the node unconditionally requests a relay reservation from the nearest known bootstrap peer. When AutoNAT later confirms `Public` reachability, the reservation is released. External addresses are discovered reactively via Identify and `ExternalAddrConfirmed` events — no manual `--external-address` flag needed.

**Justification:** Assuming private-first eliminates an operational footgun (forgetting `--nat-status private` or `--external-address` on relay servers). The relay reservation is cheap and harmless for public nodes — it gets released as soon as AutoNAT confirms reachability. This makes the system fully reactive: observe the network, don't configure it.

### 7. Length-Prefixed JSON Wire Format for Tunnels

**Decision:** Tunnel negotiation uses a 4-byte big-endian length prefix followed by a JSON payload (`TunnelRequest`/`TunnelResponse`), then raw bidirectional byte copying.

**Justification:** JSON is human-debuggable and trivially extensible (add a field without breaking existing peers). The length prefix avoids delimiter ambiguity and enables bounded reads (max 64 KB, preventing memory exhaustion). Once negotiation completes, the tunnel is zero-overhead raw TCP bridging via `tokio::io::copy_bidirectional`.

### 8. futures-to-tokio Bridging via compat()

**Decision:** libp2p streams implement `futures::io::AsyncRead/Write`, but tokio's `copy_bidirectional` requires `tokio::io::AsyncRead/Write`. We bridge with `tokio_util::compat::FuturesAsyncReadCompatExt`.

**Justification:** This is the standard approach for the futures/tokio ecosystem split. The `.compat()` adapter is zero-cost (just trait delegation) and avoids reimplementing bidirectional copy.

## Module Layout

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
```

## Sub-Behaviours

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

## Structured Events Reference

Punchgate emits structured `tracing` events at every observation point. These are the events you can expect in logs:

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
