# CLAUDE.md

Declarative functional programming standards with monadic patterns for Rust.

---

## Core Principles

### Declarative Over Imperative

Describe **what** you want, not **how** to compute it. Use transformation pipelines (`map`, `filter`, `fold`, `flat_map`) instead of loops with mutation. Start declarative; go imperative only when profiling proves necessity.

### Simplest Abstraction

Over-engineering is the primary risk in FP adoption. Always choose the minimal abstraction:

| Need                             | Abstraction | Operation  |
| -------------------------------- | ----------- | ---------- |
| Transform value                  | Functor     | `map`      |
| Combine independent computations | Applicative | `zip`      |
| Sequence dependent computations  | Monad       | `and_then` |

If your chain only uses `map`, you need a Functor—not a Monad. If computations are independent, use `zip` for parallelism—Monads serialize by design.

### Railway-Oriented Programming

Values flow along the happy path until an error diverts to the failure track. Tracks never merge implicitly—handle both paths explicitly via pattern matching or `map_err`/`unwrap_or_else`.

### Errors Are Values

Never panic for expected failures. Panics are for bugs and unrecoverable states only. Design domain errors as closed enum hierarchies with specific variants for programmatic handling.

### Immutability

Prefer immutable bindings and owned data. Rust's ownership provides stronger guarantees than runtime immutability. Use explicit immutable collections (`im`, `rpds`) only when you need cheap snapshots, structural sharing, or concurrent read access. Otherwise rely on ownership and borrowing.

### Canonical State

Persist only **irreducible source data**. Derive everything else on demand. Derived values cannot drift from their source when computed on demand.

### Pure vs Effectful

Pure functions are deterministic with no side effects—testable with simple input/output assertions, composable, cacheable, parallelizable. Side effects (I/O, network, time, randomness) belong in dedicated types that make the effect explicit in the return type.

### Composition Over Nesting

Replace nested conditionals with flat transformation chains. Pipelines read linearly; nested structures require mental stack management.

### Exhaustive Matching

All variant handling must use exhaustive pattern matching. The compiler verifies every case is handled. No default catch-alls—when you add a variant, the compiler shows every location that needs updating.

### Coalgebraic State Machines

Each state variant determines valid observations and transitions. The state type encodes what operations are valid. Invalid transitions should be unrepresentable in the type system where possible. Use exhaustive `match` on enums for runtime-validated transitions. Use typestate pattern (states as types) when invalid transitions must be compile errors—reserve for public APIs with serious misuse consequences.

### Collections Are Never Optional

Never wrap collections in `Option`. Empty collection already represents absence—`Option<Vec<T>>` is redundant since `vec![]` conveys "no items."

| Scenario                               | Use                     |
| -------------------------------------- | ----------------------- |
| Success always returns a collection    | `Vec<T>` (empty = none) |
| Need to distinguish failure from empty | `Result<Vec<T>, E>`     |
| Single value that may be absent        | `Option<T>`             |

This applies to all collection types: vectors, sets, maps, iterators.

### Code Style

- Declarative pipelines over imperative steps
- Short, focused functions named for what they return
- No comments unless code truly needs explanation
- No docstrings on self-explanatory code

---

## Dependencies

| Crate                  | Purpose                              |
| ---------------------- | ------------------------------------ |
| `tokio`                | Async runtime                        |
| `libp2p`               | Peer-to-peer networking              |
| `libp2p-stream`        | Stream-based protocol handler        |
| `thiserror`            | Typed error enums                    |
| `anyhow`               | Ergonomic error handling in binaries |
| `clap`                 | CLI argument parsing                 |
| `serde` / `serde_json` | Serialization                        |
| `tracing`              | Structured logging                   |
| `futures`              | Async combinators and utilities      |
| `tokio-util`           | Compat layer (futures ↔ tokio)       |
| `itertools`            | Extended iterator combinators        |
| `proptest`             | Property-based testing               |

---

## Error Handling

Use `thiserror` for typed error enums with specific variants for programmatic handling. Use `anyhow` in binary/CLI code where error context matters more than programmatic matching. The `?` operator is Rust's native monadic bind with early return—prefer over explicit `match` unless branch-specific logic needed.

No `unwrap()`/`expect()` in library code. Return `Result`, let caller decide.

### Provably Infallible Operations

Library code may use `expect("proof")` or `as` casts for operations **mathematically provable** to never fail. Each usage MUST include a comment proving infallibility (e.g., compile-time constants, bounded inputs, prior validation). Without proof comment, the pattern is forbidden.

### Test Code Exception

Test code (`tests/`, `examples/`, `#[test]` functions) may use `expect()` with descriptive messages for clearer test failures. Never use bare `unwrap()` in tests—always prefer `expect("context")` to explain what failed.

| Context      | Allowed         | Forbidden              |
| ------------ | --------------- | ---------------------- |
| Library code | `?`, `Result`   | `unwrap()`, `expect()` |
| Test code    | `expect("msg")` | `unwrap()`             |
| Examples     | `expect("msg")` | `unwrap()`             |

---

## No Hidden Panics

Never use operators that can panic at runtime. All fallible operations must return `Result` or `Option` to make failure explicit in the type system.

| Forbidden             | Required                                          |
| --------------------- | ------------------------------------------------- |
| `x as T`              | `T::from(x)` or `T::try_from(x)?`                 |
| `slice[i]`            | `slice.get(i)` → `Option`                         |
| `slice[a..b]`         | `slice.get(a..b)` → `Option`                      |
| `a + b`, `a * b`, etc | `a.checked_add(b)`, `a.checked_mul(b)` → `Option` |
| `a / b`, `a % b`      | `a.checked_div(b)`, `a.checked_rem(b)` → `Option` |
| `HashMap[key]`        | `map.get(&key)` → `Option`                        |

---

## Async

Use tokio as the async runtime. Use `TryFutureExt` combinators (`map_ok`, `and_then`, `or_else`) for railway-oriented async composition. Use `tokio_util::compat::FuturesAsyncReadCompatExt` for bridging futures ↔ tokio async I/O.

---

## Anti-Patterns

| Forbidden                        | Required                                  |
| -------------------------------- | ----------------------------------------- |
| `unwrap()`/`expect()` in lib     | Return `Result`                           |
| `unwrap()` in tests              | `expect("descriptive message")`           |
| `Box<dyn Error>` in public API   | Typed error enum                          |
| `clone()` to avoid borrow issues | Restructure ownership                     |
| Panic for expected errors        | Return `Result`                           |
| `as` for type conversion         | `T::from(x)` or `T::try_from(x)?`         |
| `[]` indexing                    | `.get()` returning `Option`               |
| Unchecked arithmetic             | `.checked_*()` methods returning `Option` |
| `Option<Vec<T>>`                 | `Vec<T>` or `Result<Vec<T>, E>`           |
| Default case in match            | Explicit variant handling                 |

---

## Testing Philosophy

All tests must be **property-based, generative, and integration-focused**. No trivial unit tests with hardcoded values.

### Core Rules

| Rule                       | Description                                                                   |
| -------------------------- | ----------------------------------------------------------------------------- |
| Generative inputs only     | All test data comes from generators—never hardcoded magic values              |
| Integration over isolation | Test complete protocol pipelines, not isolated internal functions             |
| State delta verification   | Capture state before/after operations, assert on deltas and invariants        |
| Real dependencies          | Use real crypto, real services—mock only unavoidable external APIs            |
| Error paths are required   | Generate invalid inputs and test all failure modes with same rigor as success |
| Properties over examples   | Assert invariants on state transitions, not exact values                      |

### Forbidden Patterns

| Forbidden                                   | Required                                              |
| ------------------------------------------- | ----------------------------------------------------- |
| Hardcoded test values                       | Generated values via `proptest`                       |
| Testing private/internal functions directly | Testing through public APIs as users would            |
| Mocking crypto or core services             | Real implementations with test configurations         |
| Single example per behavior                 | Multiple generated scenarios covering the input space |
| Asserting exact values                      | Asserting properties and invariants                   |
| Testing functions in isolation              | Testing complete flows end-to-end                     |

### Test Structure

Every test follows this pattern:

1. **Generate** — Create random valid inputs using `proptest`
2. **Setup** — Establish preconditions (create peers, configure network)
3. **Capture before** — Snapshot relevant state
4. **Execute** — Perform the operation under test
5. **Capture after** — Snapshot state post-operation
6. **Assert properties** — Verify invariants hold on the state delta

### Test Utilities

Build reusable infrastructure:

| Utility Type    | Purpose                                                          |
| --------------- | ---------------------------------------------------------------- |
| Generators      | Produce valid random inputs (peer IDs, addresses, messages)      |
| State verifiers | Capture snapshots, compute deltas, assert invariants             |
| Flow helpers    | Encapsulate multi-step operations (peer setup, connection flows) |
| Await utilities | Handle async completion, timeout handling with tokio             |
| Config builders | Construct test configurations with sensible defaults             |
