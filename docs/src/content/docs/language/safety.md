---
title: Safety
description: How Nulang guarantees memory safety, type safety, and fault tolerance at compile time.
---
import CapabilityDemo from '../../../components/animations/CapabilityDemo.astro';

## Reference Capabilities

<CapabilityDemo />

Nulang's capability system (inspired by Pony) prevents data races and use-after-free at compile time. There are seven capabilities:

| Capability | Deny Read | Deny Write | Sendable | Description |
|------------|-----------|------------|----------|-------------|
| `iso` | Yes | Yes | Yes | Isolated, unique reference |
| `trn` | No | Yes | No | Transitional, write-unique |
| `ref` | No | No | No | Mutable, shared-nothing |
| `val` | No | Yes | Yes | Immutable, shareable |
| `box` | Yes | No | No | Read-only |
| `tag` | Yes | Yes | Yes | Opaque, identity-only |
| `lineariso` | Yes | Yes | Yes | Linear isolated (at-most-once use) |

**Key guarantees**:

- **No data races**: only `iso` and `val` are sendable between actors. Mutable `ref` and `trn` cannot cross actor boundaries.
- **Compile-time only**: capabilities are erased at runtime. There is zero overhead for capability checks — they are proved by the type checker and then discarded.
- **LinearIso enforcement**: `lineariso` is tracked per binding along every control-flow path. Sending or capturing a `lineariso` value consumes it; branch-merge analysis ensures at-most-once use conservatively.

## Type System

- **Hindley-Milner inference** (Algorithm W): full type inference with polymorphism. The compiler infers types globally — you write annotations only for public APIs.
- **No `any` / `dynamic`**: every expression has a known type. There are no implicit coercions or runtime type checks.
- **Pattern matching**: variants, tuples, records, nested patterns, and guards are type-checked, but compile-time exhaustiveness analysis is not complete yet. A non-exhaustive `match` can still reach the runtime `non-exhaustive match` error path. Compile-time exhaustiveness is tracked as P0 correctness work in issue #332.
- **No null**: `nil` is an explicit tagged value with its own type (`Nil`). You cannot dereference nil — the type system tracks where `nil` may flow.
- **Row polymorphism**: records are structurally typed. A function accepting `{ x: Int, y: Int }` works with any record containing those fields (and any others) — no type-level casting needed.

## Effect System

Every function carries an effect row checked at compile time:

```nulang
// Pure — no effects
fn add(x: Int, y: Int) -> Int = x + y

// Performs IO — effect row {IO}
fn greet() -> Unit ! {IO} {
    perform IO.print("Hello")
}
```

- **Row polymorphism**: `!{IO | e}` means "IO plus whatever other effects the caller has." Effects compose without monad transformers.
- **Handler exhaustiveness**: unhandled effects are compile-time errors. If a function performs `State.get`, the caller must either handle `State` or propagate it in its own effect row.
- **Side-effect documentation**: the effect row IS the documentation. You can see every side effect a function may have by reading its type signature.

## Actor Isolation

Actors share no memory. All communication is via message passing — there is no shared mutable state between actors.

- **Mailbox isolation**: each actor has a private mailbox. Delivery can fail—for example, a bounded mailbox can overflow or a target can disappear—and those failures are routed through the runtime's failure/DLQ paths rather than silently executing another handler.
- **Per-actor GC**: ORCA garbage collection operates per-actor. One actor's GC cycle never pauses another actor — no global stop-the-world.
- **Supervision isolation**: supervision trees restart failed actors in isolation. A crashing actor's memory is released; other actors continue running.

## Fault Tolerance

Nulang inherits BEAM/OTP fault-tolerance patterns:

- **Supervision trees**: four restart strategies — `one_for_one`, `one_for_all`, `rest_for_one`, `simple_one_for_one`. Configurable restart intensity (max restarts per time window).
- **Exit trapping**: actors can trap exits from linked or monitored actors, handling failures as messages rather than crashing.
- **Process groups**: named groups of actors for coordinated shutdown or broadcast.
- **Durable workflows**: checkpointed state with saga compensation — if a workflow step fails, previously completed steps are compensated (rolled back) automatically.
- **Cascading shutdown**: when a supervisor terminates, all children are shut down in dependency order. Abnormal kills propagate to linked actors unless trapped.

## Comparisons

### vs Rust

| | Nulang | Rust |
|---|---|---|
| **Memory safety** | Capabilities + ORCA GC | Ownership + borrowing + lifetimes |
| **Lifetime annotations** | None needed | Explicit `'a` annotations |
| **Sendable by default** | `val` (immutable) is sendable; `iso` transfers ownership | `Send + Sync` traits |
| **Concurrency bugs** | Prevented by actor isolation + capabilities | Prevented by `Send`/`Sync` + borrow checker |

### vs Go

| | Nulang | Go |
|---|---|---|
| **Data race detection** | Compile-time (capabilities) | Runtime (`go run -race`) |
| **Nil safety** | Nil is typed and checked | Nil dereference panics at runtime |
| **Error handling** | Pattern matching on `Result` | `if err != nil` |
| **Effect tracking** | Compile-time effect rows | No effect system |

### vs Erlang/Elixir

| | Nulang | Erlang/Elixir |
|---|---|---|
| **Type safety** | Static types catch bugs at compile time | Dynamic types — errors surface at runtime |
| **Effect documentation** | Effect rows in type signatures | No effect tracking — any function can do I/O |
| **Pattern matching** | Typed; compile-time exhaustiveness analysis is still incomplete | Non-exhaustive by default |
| **Fault tolerance** | Same OTP supervision primitives | Same OTP supervision primitives |

### vs C/C++

| | Nulang | C/C++ |
|---|---|---|
| **Manual memory management** | None — ORCA GC per actor | `malloc`/`free`, `new`/`delete` |
| **Use-after-free** | Impossible (GC + capabilities) | Common source of CVEs |
| **Buffer overflows** | Bounds-checked arrays | Raw pointer arithmetic |
| **Data races** | Prevented by capabilities | Undefined behavior |
| **Null dereference** | Typed `nil`, checked by type system | Segmentation fault |
