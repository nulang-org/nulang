---
title: Safety
description: Current compiler checks, runtime safeguards, and limits of Nulang's safety guarantees.
---
import CapabilityDemo from '../../../components/animations/CapabilityDemo.astro';

## Reference Capabilities

<CapabilityDemo />

Nulang's Pony-inspired reference capabilities statically restrict aliasing, mutation, and actor-message transfer. They work alongside the ORCA runtime memory manager; a capability check is not, by itself, a proof that every VM, FFI, or unsafe runtime path is memory-safe. There are seven capability qualifiers:

| Capability | Deny Read | Deny Write | Sendable | Description |
|------------|-----------|------------|----------|-------------|
| `iso` | Yes | Yes | Yes | Isolated, unique reference |
| `trn` | No | Yes | No | Transitional, write-unique |
| `ref` | No | No | No | Mutable, shared-nothing |
| `val` | No | Yes | Yes | Immutable, shareable |
| `box` | Yes | No | No | Read-only |
| `tag` | Yes | Yes | Yes | Opaque, identity-only |
| `lineariso` | Yes | Yes | Yes | Linear isolated (at-most-once use) |

**Current enforcement and boundaries**:

- **Actor sends**: the analyzer permits locally sendable `iso`, `lineariso`, `val`, and `tag` values, subject to transfer/aliasing rules. `ref` and `trn` cannot be sent as mutable aliases. Remote sends also require a network-serializable representation; a locally transferable value is not necessarily remotely sendable.
- **Compile-time qualifiers versus runtime ownership**: capability qualifiers are checked statically and need no runtime *capability-tag* checks. ORCA collection, message transfer, and runtime/host validation still have costs and correctness obligations.
- **Linear use**: `lineariso` analysis conservatively tracks consumption across branches. This establishes a checked source-level rule, not an assertion that all unsafe runtime and external integrations have been formally verified.

## Type System

- **Hindley-Milner inference** (Algorithm W): polymorphic local expressions often need no annotations. Actor behavior and other externally checked boundaries still require explicit contracts.
- **Typed core**: Nulang does not expose a general `any`/`dynamic` escape hatch in its core type system. Boundary checks and dynamic runtime dispatch still exist; they should not be confused with compile-time proofs.
- **Pattern matching**: variants, tuples, records, nested patterns, and guards are type-checked, but compile-time exhaustiveness analysis is not complete yet. A non-exhaustive `match` can still reach the runtime `non-exhaustive match` error path. Compile-time exhaustiveness is tracked as P0 correctness work in issue #332.
- **Explicit `nil`**: `nil` is a tagged value with a distinct type (`Nil`), not an implicit nullable form of every reference type. Host, FFI, and experimental backend behavior requires separate validation.
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
- **Static effect-row checking**: the checker rejects a function whose inferred effects exceed an explicitly permitted row. This constrains what it *may request*; it does not prove that every dynamically performed effect has a handler installed.
- **Dynamic handler resolution**: handlers are selected at runtime. An unhandled operation can produce `Unhandled effect` at runtime; see `conformance/behavior/resume_12_unhandled_effect_runtime_error.json` and `SPEC2.md`.
- **External authority is separate**: an effect row does not grant access to a particular file, secret, or network endpoint. The runtime must enforce those grants at host boundaries. See `docs/SEMANTIC_STABILIZATION_CONTRACT.md`.
- **Effect rows as interfaces**: effect rows document checked effect requirements, not a complete account of ambient runtime, persistence, or backend behavior.

## Actor Isolation

Actors communicate through messages and are intended to prevent cross-actor mutable aliases. Some immutable or transferable values use runtime-managed representations; actor isolation therefore relies on both static sendability checks and runtime ownership handling.

- **Mailbox isolation**: actors have private mailboxes. Spawned actors currently default to unbounded mailboxes; configured capacity provides backpressure. Failed delivery and remote transport behavior must be handled explicitly.
- **ORCA memory management**: actor-local ownership and runtime-managed cross-actor references reduce broad heap coupling. Do not infer that every collector operation is independent or that there are no global coordination points.
- **Supervision**: links, monitors, restart policies, and actor isolation support containment. Actual recovery guarantees depend on the execution and persistence path.

## Fault Tolerance

Nulang inherits BEAM/OTP fault-tolerance patterns:

- **Supervision trees**: four restart strategies — `one_for_one`, `one_for_all`, `rest_for_one`, `simple_one_for_one`. Configurable restart intensity (max restarts per time window).
- **Exit trapping**: actors can trap exits from linked or monitored actors, handling failures as messages rather than crashing.
- **Process groups**: named groups of actors for coordinated shutdown or broadcast.
- **Durable workflows (experimental)**: history, checkpoints, and saga compensation exist, but compensation is not a transactional rollback of arbitrary external effects. Suspended resume and failed persistence boundaries are still being hardened (see [issue #1389](https://github.com/nulang-org/nulang/issues/1389)).
- **Cascading shutdown**: when a supervisor terminates, all children are shut down in dependency order. Abnormal kills propagate to linked actors unless trapped.

## Comparisons

### vs Rust

| | Nulang | Rust |
|---|---|---|
| **Memory management model** | Reference capabilities + ORCA GC; unsafe runtime boundaries still need validation | Ownership + borrowing + lifetimes |
| **Lifetime annotations** | None needed | Explicit `'a` annotations |
| **Sendable by default** | `val` (immutable) is sendable; `iso` transfers ownership | `Send + Sync` traits |
| **Concurrency model** | Isolated actors with checked sendability; distributed/runtime invariants under stabilization | `Send`/`Sync` plus ownership rules |

### vs Go

| | Nulang | Go |
|---|---|---|
| **Data-race strategy** | Static sendability and actor isolation; runtime enforcement still matters | Race detector plus Go memory model |
| **Nil model** | Distinct `Nil` type, with backend/boundary validation still required | Nil values on supported types; invalid dereferences panic |
| **Error handling** | Pattern matching on `Result` | `if err != nil` |
| **Effect tracking** | Compile-time effect rows | No effect system |

### vs Erlang/Elixir

| | Nulang | Erlang/Elixir |
|---|---|---|
| **Type safety** | Static types catch bugs at compile time | Dynamic types — errors surface at runtime |
| **Effect documentation** | Effect rows in type signatures | No effect tracking — any function can do I/O |
| **Pattern matching** | Typed; compile-time exhaustiveness analysis is still incomplete | Non-exhaustive by default |
| **Fault tolerance** | OTP-inspired supervision and monitors; semantics still stabilizing | Mature OTP runtime and libraries |

### vs C/C++

| | Nulang | C/C++ |
|---|---|---|
| **Manual memory management** | None — ORCA GC per actor | `malloc`/`free`, `new`/`delete` |
| **Use-after-free defense** | Managed memory and checked capabilities; raw `Value`/FFI soundness work remains | Manual ownership and unsafe pointers require discipline |
| **Buffer overflows** | Bounds-checked arrays | Raw pointer arithmetic |
| **Data-race defense** | Static actor-send restrictions plus runtime isolation | Unsynchronized C/C++ data races are undefined behavior |
| **Null handling** | Distinct `Nil` type; boundary correctness is separately validated | Invalid dereference is undefined behavior |
