---
title: Safety
description: Current Nulang safety guarantees, enforcement boundaries, and known limitations.
---
import CapabilityDemo from '../../../components/animations/CapabilityDemo.astro';

## Reference Capabilities

<CapabilityDemo />

Nulang's reference-capability system (inspired by Pony) constrains aliasing,
mutation, and which references may cross actor boundaries. There are seven
capabilities:

| Capability | Sendable | Description |
|------------|----------|-------------|
| `iso` | Yes | Isolated, unique reference |
| `trn` | No | Transitional, write-unique reference |
| `ref` | No | Mutable reference local to an isolation domain |
| `val` | Yes | Immutable, shareable reference |
| `box` | No | Read-only borrowed view |
| `tag` | Yes | Opaque identity; no dereference |
| `lineariso` | Yes | Linear isolated reference with consumption tracking |

**Current guarantees and limits**:

- **Actor sendability**: `iso`, `lineariso`, `val`, and `tag` are
  sendable. Mutable `ref`/`trn` references and borrowed `box` views may
  not cross actor boundaries.
- **Compile-time representation**: reference capabilities are erased before
  ordinary VM execution; the compiler is responsible for enforcing the
  aliasing/sendability rules.
- **Linear isolation**: `lineariso` bindings are tracked conservatively so a
  consumed value cannot be reused.
- **Reference capability is not external authority**: `iso`/`val`/etc.
  describe aliasing and mutation. Permission to access files, sockets,
  secrets, or other external resources is a separate authority model and is
  still being completed end-to-end under RFC 0019.

## Type System

- **Hindley-Milner inference**: Nulang uses Algorithm W with polymorphism,
  records, variants, and effect rows. Public and actor boundaries may still
  require explicit annotations; inference is not a promise of whole-program
  global inference.
- **Static typing**: ordinary expressions are statically typed, but runtime
  checks still exist at dynamic boundaries such as FFI, network decoding,
  tagged VM values, and unsupported/backend-specific operations.
- **Pattern matching**: patterns are type-checked, but general exhaustiveness
  checking is **not implemented yet**. A non-exhaustive `match` may compile
  and fail at runtime. Compile-time exhaustiveness is a Semantic Closure goal.
- **Typed `nil`**: `nil` is an explicit tagged value with type `Nil`;
  it is not an untyped null pointer. Runtime operations still validate values
  at boundaries where a statically-proven reference shape is unavailable.
- **Row-polymorphic records**: records are structurally typed, including open
  rows used by field-access inference.

## Effect System

Every function type carries an effect row:

```nulang
// Pure by declaration/inference
fn add(x: Int, y: Int) -> Int = x + y

// Performs IO
fn greet() -> Unit ! {IO} {
    perform IO.print("Hello")
}
```

The current implementation provides static effect-row inference and validates
an explicitly declared row against effects inferred from that body.

Important limits:

- **Interprocedural propagation is incomplete**: the compiler does not yet
  provide a complete whole-call-graph proof that every unannotated caller
  exposes every transitive effect.
- **Handler resolution is dynamic**: a performed effect is matched against the
  runtime handler stack. If no handler or runtime-backed operation handles it,
  execution raises an unhandled-effect error.
- **Handler exhaustiveness is not yet a compile-time guarantee**.
- **Durable regions add stricter rules**: effects considered unsafe for replay
  are rejected or must be modeled through explicit durable semantics.

Effect rows therefore provide useful static information today, but they should
not be read as a complete proof that arbitrary effects can never fail at
runtime.

## Actor Isolation

Actors communicate by message passing and mutable actor-owned heap state is
kept within its owning runtime shard.

- **Mailbox isolation**: each actor has its own mailbox. The implementation has
  separate system and normal lanes plus scheduler-local staging, so it is not
  a single global FIFO.
- **Backpressure**: a mailbox can be configured with a capacity for
  normal/bulk traffic. Producers receive explicit backpressure when that
  capacity is full.
- **System lane**: supervision/monitor system messages bypass the ordinary
  capacity so control traffic is not blocked by application traffic. This
  means a configured mailbox capacity is not a hard total-memory bound.
- **Per-actor memory management**: ORCA manages actor-owned objects without a
  process-wide stop-the-world collector. GC work is still runtime work on the
  owning shard and should not be interpreted as zero scheduling cost.
- **Supervision**: supervision trees, links, monitors, and restart policies
  isolate many failures, but not every BEAM/OTP primitive has a first-class
  language-level surface yet.

## Fault Tolerance

Nulang implements several BEAM/OTP-inspired runtime mechanisms:

- **Supervision trees**: `one_for_one`, `one_for_all`, `rest_for_one`,
  and `simple_one_for_one`, with restart-rate limits.
- **Links and monitors**: supported by the runtime; some operations still have
  less complete language-level ergonomics than Erlang/OTP.
- **Process groups**: runtime support exists for grouping actors.
- **Durable workflows**: state, journal entries, timers, signals, and saga
  compensation can survive process restart.
- **Compensation is not rollback**: an external side effect cannot be
  automatically undone merely because it appears in a durable step. RFC 0019
  requires replay-safe, idempotent, compensatable, or forbidden
  classifications for durable external effects.
- **Cascading shutdown**: abnormal failures can propagate through links and
  supervisors according to configured runtime semantics.

## Comparisons

These comparisons describe the current design intent and implemented safety
mechanisms; they are not claims of complete feature parity.

### vs Rust

| | Nulang | Rust |
|---|---|---|
| **Memory model** | Actor-local memory + reference capabilities + ORCA | Ownership, borrowing, lifetimes |
| **Sendability** | Capability-based actor-boundary checks | `Send` / `Sync` traits |
| **Concurrency model** | Actors and message passing | Shared-memory and message-passing libraries |
| **Exhaustive variants** | Planned compile-time exhaustiveness; not complete today | Compile-time exhaustive `match` |

### vs Go

| | Nulang | Go |
|---|---|---|
| **Race prevention** | Actor isolation + reference capabilities for supported paths | Runtime race detector plus language/library synchronization |
| **Nil model** | Explicit typed `Nil` value | `nil` for several reference-like types |
| **Error handling** | Variants, `catch`/`fail`, runtime errors | Explicit `error` values |
| **Effect tracking** | Static effect rows with current limitations | No language-level effect system |

### vs Erlang/Elixir

| | Nulang | Erlang/Elixir |
|---|---|---|
| **Type system** | Static HM-style types | Dynamic |
| **Effect metadata** | Effect rows | No language-level effect rows |
| **Pattern matching** | Typed, not yet generally exhaustiveness-checked | Runtime pattern matching |
| **Fault tolerance** | BEAM-inspired supervision/link/monitor primitives | Mature OTP/BEAM primitives |

### vs C/C++

| | Nulang | C/C++ |
|---|---|---|
| **Ordinary memory management** | Managed actor heaps / ORCA | Manual and RAII-based native memory |
| **Actor isolation** | Compiler/runtime-enforced sendability for supported reference capabilities | Library/design dependent |
| **Array access** | Runtime/compiler checked according to the active backend | Raw pointer arithmetic is possible |
| **FFI/native escape hatch** | Native FFI is explicitly unsafe/trusted and policy-gated | Native code is the default execution model |

## Security vs. Type Safety

Reference capabilities, effect rows, and static types are not substitutes for
security authority. External-resource access is being separated into typed
`AuthorityGrant` / `AuthorityManifest` concepts. Until RFC 0019's authority
plumbing is complete, do not treat the presence of capability syntax as an
end-to-end sandbox guarantee.
