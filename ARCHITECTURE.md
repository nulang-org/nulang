# Nulang Architecture Reference

**Document Version:** 1.1
**Date:** July 2026
**Audience:** Core implementers, runtime engineers, language designers
**Companion Documents:** design notes in `docs/archive/` (AI SDK, workflow SDK, cloud, package manager)

> **Implementation status (v1.1):** Sections 2 and 6 (Language and AI Runtime)
> have been re-verified against the current source tree and describe the
> system as implemented. Sections 3–5 and the derived material in Sections
> 7–10 still describe the *target* architecture; where the implementation
> already diverges in a verified way, §1 carries the corrections. Treat
> uncaveated numbers and diagrams in Sections 3–5 as design goals, not
> as-built fact.

---

## Table of Contents

1. [System Overview](#1-system-overview)
2. [Layer 1: Language](#2-layer-1-language)
3. [Layer 2: Actor Runtime](#3-layer-2-actor-runtime)
4. [Layer 3: Durable Execution](#4-layer-3-durable-execution)
5. [Layer 4: Distributed Platform](#5-layer-4-distributed-platform)
6. [Layer 5: AI Runtime](#6-layer-5-ai-runtime)
7. [Data Flow Diagrams](#7-data-flow-diagrams)
8. [Component Interaction Map](#8-component-interaction-map)
9. [Architecture Decision Records](#9-architecture-decision-records)
10. [Performance Targets](#10-performance-targets)
11. [References](#11-references)

---

## 1. System Overview

Nulang is organized into five strictly layered subsystems. Each layer communicates only with adjacent layers. This constraint ensures independent testability, replaceability, and evolution.

```
+==========================================================================+
|                           LAYER 5: AI RUNTIME                             |
|  LLM Providers | Agent Actors | Tool Schemas | Memory (3 kinds) | Cost   |
|  Pipelines | Supervisor Teams | Debates | Sync-over-Async Providers      |
+==========================================================================+
                              |
+==========================================================================+
|                       LAYER 4: DISTRIBUTED PLATFORM                       |
|  Gossip Membership | CRDT Engine | Message Router | Service Discovery   |
|  Consistent Hash Ring | Multi-Region Replicator | NUL0 Transport        |
+==========================================================================+
                              |
+==========================================================================+
|                       LAYER 3: DURABLE EXECUTION                          |
|  State Models (4) | Checkpoint Manager | Event Journal | Recovery        |
|  Snapshot Engine | Replay Orchestrator | Determinism Enforcer           |
+==========================================================================+
                              |
+==========================================================================+
|                       LAYER 2: ACTOR RUNTIME                              |
|  Virtual Actor Manager | Work-Stealing Scheduler | Bounded Mailboxes    |
|  Supervision Trees | WASM Sandbox Pool | Location Directory | ORCA GC   |
+==========================================================================+
                              |
+==========================================================================+
|                       LAYER 1: LANGUAGE                                   |
|  Lexer | Parser | HM Type Checker | Effect & Capability Analysis | HIR   |
|  MIR | Register-Bytecode Compiler | Tagged-Value VM | Cranelift JIT Tier |
+==========================================================================+
```

**Layer boundary rules:**
- Layer N may call only Layer N-1 and Layer N+1
- Cross-layer calls are forbidden (no Layer 5 → Layer 3 shortcuts)
- Data passes across boundaries as plain structs; no shared mutable state
- Each layer can be tested with mocked adjacent layers

**Implementation note (updated July 2026):** the compiler targets a
register-VM bytecode by default. A WASM backend (`--backend wasm | wasm-run | wasm-aot`)
exists behind the `wasm-backend` feature flag (src/mir_wasm.rs, src/wasm_runtime.rs).
A native/AOT backend (`--backend native`) compiles via Cranelift ahead-of-time.
Other verified divergences from the target design of Layers 2–4, to keep in
mind while reading §3–§5:

- **Mailboxes are unbounded**, backed by `crossbeam::queue::SegQueue`
  (`src/runtime/mailbox.rs`); push always succeeds — there is no 10,000-slot
  ring buffer, no overflow policy, and no transport-level backpressure.
  Messages carry a `MessagePriority` (`System`/`Normal`/`Bulk`) field, but the
  queue itself is a single FIFO.
- **Actor identity is a bare `u64`** from a global atomic counter
  (`fresh_actor_id`, `src/runtime/mod.rs`); `spawn` is explicit — there is no
  Orleans-style string identity, no activation-on-first-message, and no
  consistent-hash placement yet.
- **The reduction budget is 1000** per scheduling round (`next_reductions` in
  `src/runtime/mod.rs`), enforced by a synchronous single-threaded
  `step_actor` loop — not 2000 WASM-asyncify reductions.
- **Persistence** ships three `PersistenceStore` backends — `MemoryStore`,
  `JsonFileStore`, `LibsqlStore` (`src/runtime/persistence.rs`) — not
  PostgreSQL or S3.
- **The NUL0 transport** is a hand-rolled, length-prefixed TCP protocol whose
  fixed header is 13 bytes (4-byte `NUL0` magic, 1-byte packet type, 8-byte
  sequence); the version/flags/MAC fields, TLS, QUIC, and Poly1305 drawn in
  §5.5 do not exist in `src/runtime/network.rs`.

---

## 2. Layer 1: Language

The Language layer is a self-contained compiler: Nulang source in,
register-machine bytecode out, executed by an interpreter loop with a
Cranelift JIT tier. It has no hard dependency on the actor runtime — the VM
reaches actors only through two object-safe callback traits (§2.6), so the
language layer runs standalone (`VM::new()` + `load_module` + `run`), which
is how `--eval` and the REPL work without a cluster attached.

### 2.1 Syntax Design

Nulang is an expression-oriented, ML-flavored language with C-style braces
for blocks and actor bodies. Indentation is **not** significant — the lexer
preserves newlines as tokens only so the parser can terminate expressions.
Every construct is an expression that produces a value; there is no `return`
keyword, and the last expression of a block is the block's value. Comments
are `//`. The examples in this section are taken from `examples/` and run as
written.

```nulang
// Recursive Fibonacci — closures and recursion (examples/fibonacci.nula)
let fib = fn(n) {
    if n <= 1 then n else fib(n - 1) + fib(n - 2)
} in fib(10)
```

Core surface forms (`src/ast.rs`, `src/parser.rs`):

- `let x = e1 in e2` and `let rec f = ... in ...`. A `let`-bound lambda may
  reference itself; it is lowered like `let rec` so the self-reference
  resolves.
- `fn(x) { body }` lambdas with real closures (captured locals live in
  closure environments, §2.6).
- `if cond then e1 else e2`; `match scrutinee { | Pat => e ... }`; blocks
  `{ e1; e2 }`; tuples, records `{ field: value }`, arrays.
- Actors: `actor Counter { state count = 0 behavior inc() { self.count + 1 } }`,
  spawned explicitly with `spawn Counter { count = 0 }`
  (`examples/counter_actor.nula`). Messages are sent with
  `send target behavior(args)`; inside a behavior,
  `receive { | Msg(x) => x }` reads the next mailbox message
  (`examples/receive.nula`).
- Effects: `perform Effect.op(args)`, intercepted by handlers written as
  `handle perform Math.getAnswer() { | Math.getAnswer() => 42 }`
  (`examples/effects.nula`).
- Module-level declarations: `fn` (optionally `@tool`-annotated, §6.5),
  `actor`, `type` aliases, record and variant types, `effect` declarations,
  `module`/`import`, `extern` blocks, `workflow` (v0.8), and `agent` (v0.9,
  §6.3).

There is no `switch` and no `case` keyword — `match` arms are introduced by
`|`. Pattern matching is typed but **not** exhaustiveness-checked today: a
non-exhaustive `match` compiles and may fail at runtime.

### 2.2 HM Type Inference

`TypeChecker::check_module` (`src/typechecker.rs`) implements Damas-Milner
Algorithm W in the classical substitution-based form:

- `Substitution = Vec<(TypeVar, Type)>`; unification via `mgu` with an occurs
  check; `apply_subst` / `compose_subst` propagate results.
- `generalize` at let-bindings builds `Type::Scheme { vars, body }`;
  `instantiate` freshens scheme variables at use sites.
- The `Type` universe (`src/types.rs`): `Var`, `Primitive` (`Int`, `Float`,
  `Bool`, `String`, `Nil`, `Unit`, `Never`, `Address`), `Tuple`, `Record`,
  `Variant`, `Array`, `Function { param, ret, effect, cap }`,
  `Actor { state, behavior }`, `App { constructor, args }` (type
  constructors such as `Option[T]`), `Reference { cap, inner }`, `Scheme`.

**Parameterized types.** Functions, actors, and type declarations accept
`type_params` (e.g. `fn f[T](x: T)`). Because the VM's value representation
is dynamically tagged (§2.6), generic types are **erased**, not
monomorphized — no per-instantiation code is generated, and there is no
special boxing of generic values because every value is already a tagged
`u64`.

**Row-polymorphic effects.** Every function type carries an `EffectRow`:
`Closed(Vec<Effect>)` or `Open(Vec<Effect>, Region)` (`src/types.rs`). See
§2.3 for checking and runtime dispatch.

**Capability types.** Function types also carry a `Capability` from the
Pony-style lattice (`iso`, `lineariso`, `trn`, `ref`, `val`, `box`, `tag`;
subtyping computed via `join`). `LinearIso` adds exactly-once linear
consumption tracking. Capabilities are compile-time only — see §2.4.

**What does not exist:** no type classes or constrained types
(`fn f[T: Serializable]` is not valid Nulang), no `protocol` construct, and
no exhaustiveness checking for `match`. Type inference deliberately does not
cross actor boundaries — behavior signatures are explicit annotations — so
actors remain separately checkable units.

### 2.3 Effects: Static Rows, Runtime Handler Stack

Effects are row-polymorphic in the type system and handler-resolved at
runtime; the two halves meet in the `Perform` opcode.

**Static side** (`src/effect_checker.rs`, wired per declaration in
`run_frontend`, `src/main.rs`):

- `EffectChecker::infer_effects` computes the row a body may perform;
  `check_effects` enforces a declared row (`fn f() ! E { ... }`) against the
  inferred one via `effect_row_subset`. Rows are `Closed` or `Open` with a
  `Region` variable; an open row on the *allowed* side may cover extra
  effects.
- Bodies with a declared row are enforced; un-annotated bodies are
  inference-only so existing programs keep compiling until interprocedural
  effect propagation lands. The checker accumulates `diagnostics` rather
  than aborting the compile.

**Runtime side** (opcode-level detail in §2.6):

- `handle perform Math.getAnswer() { | Math.getAnswer() => 42 }` compiles to
  a handler table plus a `Handle` opcode; `perform` compiles to `Perform`; a
  handler that resumes the suspended computation uses `Resume`; leaving the
  handled scope emits `Unwind`.
- If no installed handler binds the effect, the VM asks the
  `ActorVmCallbacks::perform_effect` hook. This is how built-in effects are
  serviced — e.g. `Timer.sleep` in workflow steps and `Python.call(...)`.
  Any waiting on I/O happens inside the hook implementation, never in the VM
  interpreter loop itself.
- `perform LLM.ask(prompt)` never reaches the generic path: MIR lowering
  dispatches it through the generic `PerformAsync` (0xC6) effect opcode with
  the `"Inference.ask"` effect-op string (§6.3).

**Determinism.** The static row tells the durable-execution layer (§4) which
operations a body *may* perform; capturing and replaying effect results for
deterministic replay is the persistence layer's job, not the effect
checker's.

### 2.4 Reference Capabilities (Compile-Time Only)

"Capabilities" in the implemented language are Pony-style *reference
capabilities* — qualifiers on references that govern aliasing and
sendability — not the authority tokens of the target design. The lattice
(`src/types.rs`):

```
iso ─ lineariso ─ trn ─ ref ─ val ─ box ─ tag
```

| Capability | Meaning | Sendable? |
|-----------|---------|-----------|
| `iso` | Unique ownership | Yes |
| `lineariso` | Unique ownership + exactly-once linear consumption | Yes |
| `trn` | Unique writer (recoverable to `iso`) | No |
| `ref` | Shared read/write | No |
| `val` | Immutable shared | Yes |
| `box` | Read-only view (any non-`tag` cap reads as `box`) | No |
| `tag` | Opaque identity only, no dereference | Yes |

`CapabilityAnalyzer::infer_cap` (`src/effect_checker.rs`) runs in
`run_frontend` over the same bodies as the effect checker; subtyping is
computed via `join` on the lattice, and `is_sendable` is
`LinearIso | Iso | Val | Tag`.

**Capabilities are erased at runtime.** There are no capability opcodes at
all: `mir::RValue::CapabilityCheck` compiles to `Const1` (`true`) in
`src/mir_codegen.rs`, so nothing capability-related reaches the VM. There is no
runtime revocation list, no capability tokens in message headers, and no
cross-node capability verification today. When §3–§5 below mention
capability managers, narrowing, or revocation, they are describing the
target design, not the current implementation.

### 2.5 Compilation Pipeline

The pipeline is a straight line, wired exactly in `run_frontend` /
`run_source` (`src/main.rs`) and reused by the REPL (`src/repl.rs`) and the
LSP (`src/lsp/`):

```
source &str
  -> Lexer::lex()                  Vec<Token>        src/lexer.rs
  -> Parser::parse_module()        AstModule         src/parser.rs
  -> TypeChecker::check_module()   Type              src/typechecker.rs (Algorithm W)
  -> EffectChecker (per body)      infer / check     src/effect_checker.rs
  -> CapabilityAnalyzer (per body) Capability        src/effect_checker.rs
  -> hir_lower::lower_module()     hir::Module       src/hir_lower.rs
  -> mir_lower::lower_module()     mir::Module       src/mir_lower.rs
  -> mir_codegen::compile_mir()    CodeModule        src/mir_codegen.rs
  -> VM::load_module() + VM::run() Value             src/vm.rs
```

Notes:

- `--check` runs `check_source`, which stops after capability analysis — no
  HIR/MIR/codegen and no execution.
- The pipeline is **MIR-exclusive**: there is no AST-to-bytecode compiler
  left in the tree. HIR mirrors the AST with resolved types and flattened
  statement/terminator bodies, and desugars `workflow` and `agent`
  declarations into persistent actors (§6.3). MIR is 3-address code with
  explicit basic blocks and terminators — the last IR before bytecode.
- Constructs the MIR pipeline cannot yet lower return an honest
  `NotYetImplemented` error rather than miscompiling (current examples: a
  `workflow` or `agent` that reaches MIR un-desugared, and an `agent` whose
  `tools` list names an unknown function, §6.3).
- Top-level expressions are wrapped in a synthetic `__main` function.

### 2.6 Bytecode VM Backend

**Instruction format** (`src/bytecode.rs`): fixed 32-bit instructions
`{ opcode: u8, op1: u8, op2: u8, op3: u8 }` with `encode`/`decode`;
`imm16()` reads op1+op2, `simm16()` its signed form, and `offset16()` reads
op2+op3 (used by `JmpT`/`JmpF`, whose op1 holds the condition register).
Constants live in a per-module pool (`Constant::{Int, Float, String, Bool,
Nil, Unit, TypeDescriptor, FunctionRef, BehaviorRef}`). A `CodeModule` holds
the instructions, the constant pool, `function_table` (code offsets),
`exports`, an optional `entry_point`, `handler_tables` (one per `handle`
block), `actor_metadata` (`ActorMeta`, incl. the agent flags in §6.3),
`foreign_functions`, and `tools` (`@tool` schemas, §6.5).

**Opcode space:** 133 opcodes (`OpCode`, `src/bytecode.rs`):

| Range | Category | Count | Examples |
|-------|----------|------:|----------|
| 0x00–0x08 | Special | 9 | `Nop`, `Halt`, `Panic`, `Const0`…`ConstL` |
| 0x10–0x15 | Stack & locals | 6 | `Load`, `Store`, `Move`, `Dup`, `Swap` |
| 0x20–0x2D | Integer arithmetic | 14 | `IAdd`…`IPow`, shifts, bitwise |
| 0x30–0x38 | Float arithmetic | 9 | `FAdd`…`FMod`, `IToF`, `FToI`, `FToS` |
| 0x40–0x4B | Comparison & logic | 12 | `ICmp*`, `FCmp*`, `SCmpEq`, `Not/And/Or` |
| 0x50–0x57 | Control flow | 8 | `Jmp`, `JmpT/F`, `Switch`, `Call`, `TailCall`, `Ret*` |
| 0x60–0x64 | Closures | 5 | `Closure`, `CapLoad/Store`, `FreeVar`, `ClosureCall` |
| 0x70–0x7F | Memory & objects | 16 | `Alloc`, `Field*`, `Arr*`, `Tuple*`, `Rec*`, `IsTag`, `Unpack`, `Copy`, `Drop` |
| 0x80–0x8F | Actor & concurrency | 16 | `Spawn`, `Send`, `Ask`, `Receive`, `Monitor`…`Unlink`, `StateGet/Set`, `Emit`, `SignalWait`, `ReceiveMatch` |
| 0x90–0x93 | Effects | 4 | `Perform`, `Handle`, `Resume`, `Unwind` |
| 0x94–0x9B | Python interop | 8 | `PyImport`…`PyRelease` |
| 0x9C | Direct effect dispatch | 1 | `PerformDirect` |
| 0xA0–0xA1 | Actor concurrency (cont.) | 2 | `ReceiveWait`, `ReceiveCommit` |
| 0xB0 | FFI | 1 | `FFICall` |
| 0xC6 | Async effect dispatch | 1 | `PerformAsync` |
| 0xD0–0xD5 | Distribution | 6 | `NodeId`, `Migrate`, `RSend`, `RAsk`, `RSpawn`, `Gossip` |
| 0xE0–0xE7 | String & IO | 8 | `SConcat`, `SPrint/SRead`, `FOpen`…`FClose`, `Print` |
| 0xF0–0xF4 | Debug & meta | 5 | `DbgBreak/Print/Stack`, `MetaType/Cap` |
| 0xF5–0xF6 | Register spill | 2 | `SpillLoad`, `SpillStore` |

**Value representation:** canonical tagged `u64` (`Value { raw: u64 }`,
`src/vm.rs`). The public contract is `nulang.value/i64-tagged-v1`; the
canonical layout lives in **`src/value_layout.rs`** and is imported by the VM,
JIT runtime helpers, typed JIT compiler, WASM backend, and Python marshalling
layer:

- Tagged values reserve the upper 16 bits for the tag
  (`TAG_MASK = 0xFFFF_0000_0000_0000`) and the low 48 bits for payload
  (`PAYLOAD_MASK = 0x0000_FFFF_FFFF_FFFF`).
- Finite floats and infinities retain raw IEEE-754 bits. Every NaN-producing
  path canonicalizes to `CANONICAL_NAN_BITS = 0xFFF8_0000_0000_0001`, which
  cannot alias a runtime tag.
- Tags: `TAG_OBJECT 0x7FF5`, `TAG_CLOSURE 0x7FF7`, `TAG_NIL 0x7FF8`,
  `TAG_UNIT 0x7FF9`, `TAG_BOOL 0x7FFA`, `TAG_INT 0x7FFB` (signed i48),
  `TAG_PTR 0x7FFC`, `TAG_ACTOR 0x7FFD`, and `TAG_STRING 0x7FFE`.
  `TAG_PYTHON 0x7FF6` lives in `src/python/bridge.rs` behind the `python`
  feature.

**Frames:** a `Frame` holds `[Value; 256]` registers, `pc`, `module_idx`,
`return_dst`, `caller_idx`, and an optional `closure_env`. Frames live in a
flat `Vec<Frame>` on the VM (not a linked stack): a call pushes a frame
recording the caller's index, `Ret`/`RetVal` pop back. Closures with
captures are `TAG_CLOSURE` values whose payload indexes a VM-side
`closure_envs` table.

**Effects at runtime** (`handler_stack: Vec<HandlerFrame>`, `src/vm.rs`):

- `Handle` pushes a `HandlerFrame` (handler-table index, module, resume
  pc/dst).
- `Perform` finds the innermost handler frame whose table binds the effect
  name, captures a `Continuation` (deep-cloned frames up to the current one,
  resume pc/dst, step count) into that frame, and jumps to the handler body.
  With no binding it falls back to the innermost table's `fallback_offset`,
  then to the `perform_effect` callback (built-ins), else raises
  `Unhandled effect`.
- `Resume` restores the captured continuation with the resume value in the
  destination register (an error if none was captured); `Unwind` pops the
  handler frame.

**Runtime callbacks:** the VM never touches the actor runtime directly. Two
object-safe traits mediate (`src/vm.rs`): `ActorVmCallbacks` (heap
alloc/drop, spawn/send/ask, `try_receive`, `perform_effect`, and the AI hooks
`complete_llm`, `pipeline_*`, `supervisor_*`, `debate_*`) and
`DistributedVmCallbacks` (node id, migrate, remote send/ask/spawn, gossip).
Standalone execution installs `StandaloneVmCallbacks`, which owns a private
`ActorHeap`; the actor runtime installs bridges back into `Runtime`.

**MVP stubs worth knowing about:** capability checks compile to `Const1`
(§2.4); the `Py*` opcodes error in the standalone VM ("Python opcodes require
native actor runtime — use `perform Python.call(...)`"); `Receive` stores the
next message's first payload value or `nil` and serves as the non-blocking
fallback when selective receive (`ReceiveMatch` 0x8F, shipped — see
`lower_receive` in `src/mir_lower.rs`) finds no matching arm;
`FOpen`/`FRead`/`FWrite`/`FClose` are stubs.

**JIT tiering** (`src/jit/`): the VM keeps an optional `JitSession`. Before
each instruction it snapshots the frame registers into a `[u64; 256]` and
calls `jit::tiered_execute_step_typed`:

1. If a compiled function exists for `(module, pc)`, run it — its ABI is
   `extern "C" fn(*mut u64 regs, *const u64 constants)`.
2. Otherwise bump the hot counter; at `HOT_THRESHOLD = 1000` executions,
   `find_compilable_region` collects up to 500 instructions (stopping at the
   first non-compilable opcode, and *before* `Ret` so the VM still pops the
   frame); regions of 3 or more instructions (`region_len >= 3`) are compiled.
3. SIMD first: `simd_analyzer` recognizes element-wise binop/unary/compare
   loops and `simd_compiler` emits vectorized Cranelift IR
   (`I64x2`/`F64x2`/`I32x4`/`F32x4`); otherwise the scalar
   `compiler::compile_region` path is used.
4. The **typed path is live**: at tier-up, `typed_compiler::infer_reg_types`
   recovers register types from the enclosing function's bytecode (a
   conservative forward must-analysis), and hot regions compile through
   `compile_region_typed` with tag guards stripped when types are
   provable — falling back to the scalar path on absent/empty metadata or
   compile error.
5. Helpers callable from JIT code are `extern "C"` functions in
   `src/jit/runtime.rs`, tag-aware (e.g. division by zero yields `nil`).

Cold code always interprets; there is no whole-module AOT compilation.

---

## 3. Layer 2: Actor Runtime

> **Target design — read with care.** Sections 3–5 describe the architecture
> Nulang is building toward; they have not been re-verified in this revision.
> The implementation has already diverged in the verified ways listed in §1 —
> most importantly, actors execute as register-VM bytecode (§2.6) and nothing
> runs inside a WASM sandbox, and mailboxes are unbounded `SegQueue`s whose
> push always succeeds. Until these sections are re-verified against
> `src/runtime/`, treat their diagrams and numeric defaults as design intent.

The Actor Runtime is the in-memory execution engine. It schedules actors, routes messages, manages lifecycles, and enforces isolation. All code runs inside WASM sandboxes provided by Wasmtime.

### 3.1 Virtual Actor Model

Nulang implements the Orleans virtual actor model. An actor exists whenever it has an identity. There is no explicit creation or destruction.

**Actor identity:** Every actor has a unique identity string, typically `<type>:<key>`. Examples: `OrderActor:order-42`, `UserSession:session-abc123`, `PaymentGateway:stripe`. Identity is the only thing you need to send a message — no PID, no handle, no reference.

**Addressing:**

```
+------------------+     +------------------+     +------------------+
|  Sender Actor    |     |  Runtime         |     |  Target Actor    |
|                  |     |                  |     |                  |
|  send(msg, to=   | --> |  1. Parse target | --> |  (may not exist  |
|  "Order:ord-42") |     |     identity     |     |   in memory yet) |
+------------------+     |  2. Look up in   |     +------------------+
                         |     directory    |              ^
                         |  3. If inactive: |              |
                         |     activate     |--------------+
                         |  4. Deliver to   |
                         |     mailbox      |
                         +------------------+
```

**Activation lifecycle:**

1. **Activation trigger:** A message arrives for actor identity `I`
2. **Directory lookup:** The location directory maps `I` to a node (via consistent hash)
3. **Local check:** Is `I` already activated on this node?
   - Yes: route to mailbox
   - No: proceed to activation
4. **Activation:**
   - Load actor code (WASM component)
   - Restore state from latest checkpoint (if `durable` or `event_sourced`)
   - Initialize actor state (constructor runs)
   - Register in local activation table
   - Deliver message to mailbox
5. **Deactivation** (triggered by inactivity timeout or memory pressure):
   - Finish processing current message
   - Checkpoint state (if persistent)
   - Release WASM instance back to pool
   - Remove from activation table
   - Directory entry marked "inactive"

**Activation table** (per node):

```
+------------------+----------+----------+------------------+----------+
| Actor Identity   | WASM     | Mailbox  | State            | Last     |
|                  | Instance | Ref      | (running/        | Active   |
|                  | Ref      |          |  suspended)      | (ms ago) |
+------------------+----------+----------+------------------+----------+
| Order:ord-42     | wasm-17  | mbox-33  | running          | 12       |
| User:sess-abc    | wasm-4   | mbox-12  | suspended        | 4500     |
| Cart:cart-99     | wasm-17  | mbox-45  | running          | 3        |
+------------------+----------+----------+------------------+----------+
```

WASM instances are shared across actors of the same type via copy-on-write. The instance pool keeps recently-used instances warm.

**Placement strategy:** Actors are placed via consistent hashing on their identity string. This means:
- The same actor identity always maps to the same node (within replication factor)
- Node additions cause only 1/N actors to migrate (where N = node count)
- No central coordinator needed for placement decisions
- Hot actors can be manually migrated via the management API

### 3.2 Scheduler: Work-Stealing M:N with Reduction Counting

The scheduler maps M actors onto N OS threads. It is the execution heart of the runtime.

**Architecture:**

```
+------------------------------------------------------------------------+
|                          OS Thread Pool (N threads)                     |
|  Typically N = number of CPU cores                                     |
+------------------------------------------------------------------------+
|                                                                         |
|  +----------+  +----------+  +----------+         +----------+         |
|  | Sched 0  |  | Sched 1  |  | Sched 2  |  ...    | Sched N-1|         |
|  |          |  |          |  |          |         |          |         |
|  | Run Queue|  | Run Queue|  | Run Queue|         | Run Queue|         |
|  | [A1, A3] |  | [A7]     |  | [A2, A5] |         | [A4, A6] |         |
|  |          |  |          |  |          |         |          |         |
|  | LIFO for |  | LIFO for |  | LIFO for |         | LIFO for |         |
|  | spawns   |  | spawns   |  | spawns   |         | spawns   |         |
|  | FIFO for |  | FIFO for |  | FIFO for |         | FIFO for |         |
|  | messages |  | messages |  | messages |         | messages |         |
|  +----------+  +----------+  +----------+         +----------+         |
|       |             |             |                      |              |
|       | steal <-----+----- steal +----- steal -----------+              |
|       |    (when empty, steal half from random neighbor)                |
+------------------------------------------------------------------------+
```

**Per-scheduler data structures:**

| Structure | Purpose | Access Pattern |
|-----------|---------|----------------|
| Local run queue | Ready actors | LIFO (spawns) + FIFO (messages), producer-consumer |
| Steal deque | Work available for other schedulers | Lock-free Chase-Lev deque |
| Current actor | Actor currently executing | Single owner |
| Reduction counter | Remaining reductions for current actor | Decremented per operation |

**Reduction counting:** Each actor is assigned a reduction budget (default: 2000 reductions) when scheduled. One reduction ≈ one WASM instruction or one host function call. When the counter reaches zero:
1. The actor yields (WASM execution paused via asyncify)
2. Actor returns to the run queue
3. Scheduler picks the next actor

Reduction counting serves two purposes: fairness (no actor monopolizes a thread) and checkpointing (yield points are natural checkpoint boundaries).

**Work-stealing algorithm** (Chase-Lev, lock-free):

```
When scheduler S has no work:
  1. Pick a random victim scheduler V
  2. Attempt to steal half of V's deque (from the bottom)
  3. If steal succeeds: process stolen work
  4. If steal fails: try another victim (up to 3 attempts)
  5. If all fail: park the OS thread (futex wait)
  6. Wake when new work arrives (futex wake)
```

**Message processing loop:**

```
for each scheduled actor:
  1. Dequeue next message from actor's mailbox
  2. If mailbox empty: deactivate actor (if idle timeout exceeded)
  3. Load reduction counter (default 2000)
  4. Enter WASM sandbox, call behavior handler with message
  5. Handler runs until completion OR reduction counter hits 0
  6. If counter hit 0: save WASM stack (asyncify), requeue actor
  7. If handler completed: check for outgoing messages, deliver them
  8. If persistent actor: trigger checkpoint (async, non-blocking)
  9. If more messages in mailbox: requeue actor
  10. If mailbox empty: mark idle, start idle timer
```

### 3.3 Mailbox Design: Bounded MPSC with Backpressure

Every actor has exactly one mailbox. The mailbox is a bounded multi-producer, single-consumer queue.

**Mailbox structure:**

```
+------------------------------------------------------------------+
|                         Actor Mailbox                             |
|                                                                   |
|  +----------------------------------------------------------+    |
|  |  Message Queue (ring buffer, bounded)                     |    |
|  |  +------+------+------+------+------+------+------+      |    |
|  |  | Msg1 | Msg2 | Msg3 |      |      | Msg7 | Msg8 |      |    |
|  |  +------+------+------+------+------+------+------+      |    |
|  |     ^                       ^                             |    |
|  |     | head (consumer)       | tail (producers)            |    |
|  +----------------------------------------------------------+    |
|                                                                   |
|  Capacity: 10,000 (default)                                       |
|  Backpressure strategy: drop oldest / block sender (configurable) |
|                                                                   |
|  Priority bands:                                                  |
|  +----------------------------------------------------------+    |
|  | [HIGH: system messages] [NORMAL: user messages] [LOW:   |    |
|  |  batch work]                                             |    |
|  +----------------------------------------------------------+    |
+------------------------------------------------------------------+
```

**Mailbox properties:**

| Property | Value | Rationale |
|----------|-------|-----------|
| Default capacity | 10,000 messages | Prevents unbounded memory growth |
| Max message size | 1MB | Larger data uses blob store references |
| Queue type | Lock-free ring buffer | MPSC, cache-friendly |
| Priority levels | 3 (high, normal, low) | System vs user vs batch |
| Overflow policy | Configurable: `block`, `drop_oldest`, `drop_newest` | `block` for correctness; `drop` for availability |
| Fairness | Round-robin across senders | Prevents single sender from starving others |

**Backpressure:** When a mailbox is full, the sender is blocked (for local sends) or receives a `MailboxFull` error (for remote sends). The sender can retry with exponential backoff or route to a dead-letter actor. This is the only backpressure mechanism in Nulang — there are no rate limiters, no flow control protocols, just bounded mailboxes.

**Message format:**

```
+--------+--------+--------+---------+---------+----------+
|  Meta  | Priority| Sender | Target  | Payload | Cap     |
|  (16B) | (1B)    | ID     | ID      | (var)   | Tokens  |
+--------+--------+--------+---------+---------+----------+

Meta: timestamp (8B), correlation ID (4B), flags (4B)
Payload: Cap'n Proto serialized message
Cap Tokens: list of capability tokens (signed JWTs)
```

Messages are serialized to Cap'n Proto for zero-copy deserialization within the WASM sandbox. Cap'n Proto was chosen over Protocol Buffers because it requires no parsing step — the serialized bytes are the in-memory layout.

### 3.4 Supervision Trees

Every actor has a supervisor. Supervisors are themselves actors. Supervision creates a tree of accountability.

**Supervision strategies** (from Erlang/OTP):

| Strategy | Behavior | Use Case |
|----------|----------|----------|
| `OneForOne` | Restart only the failed child | Independent workers |
| `OneForAll` | Restart all children when one fails | Tightly coupled group |
| `RestForOne` | Restart failed child + all children started after it | Dependency chain |

**Restart policies:**

```nulang
actor OrderSupervisor:
  supervise:
    strategy: OneForOne
    max_restarts: 5        -- max 5 restarts in 10 seconds
    max_seconds: 10
    children:
      - OrderProcessor { restart: permanent }   -- always restart
      - PaymentGateway { restart: transient }   -- restart only on crash
      - AuditLogger    { restart: temporary }   -- never restart
```

**Escalation:** If restart limits are exceeded, the supervisor itself fails, and its own supervisor handles it. This creates a fault propagation tree:

```
Root Supervisor (realm level)
    |
    +-- Service Supervisor (one per service type)
    |       |
    |       +-- OrderActor:order-42 (worker)
    |       +-- OrderActor:order-43 (worker)
    |       +-- OrderActor:order-44 (worker)
    |
    +-- System Supervisor (runtime services)
            |
            +-- Mailbox Manager
            +-- Checkpoint Manager
            +-- Location Directory
```

**Restart sequence:**
1. Child actor crashes (WASM trap, panic, or explicit `fail`)
2. Supervisor receives `EXIT` signal with reason
3. Supervisor checks restart policy:
   - `normal` exit: no restart (unless `permanent`)
   - `crash` exit: restart (if under limit)
   - `killed` exit: restart (if under limit)
4. If under restart limit: stop child, reload WASM, restore state, start child
5. If over limit: supervisor exits with `shutdown`, escalates to parent

**Process isolation:** Each actor runs in its own WASM sandbox instance. Memory is isolated — actor A cannot read actor B's linear memory. If actor A crashes (segmentation fault, infinite loop, stack overflow), only actor A is affected. The runtime detects the crash via Wasmtime's trap handling and restarts the actor via its supervisor.

### 3.5 Per-Actor Heaps and ORCA GC

Nulang uses ORCA (Ownership and Reference Counting based on Cycles in Actors) garbage collection. ORCA is a concurrent, per-actor garbage collector designed specifically for actor systems.

**Why ORCA:** Traditional GC (stop-the-world, generational) is a poor fit for actor systems because:
- Stop-the-world pauses break real-time message processing guarantees
- Cross-actor references are hard to trace (actors are distributed)
- Per-actor collection is naturally parallel (each actor collects independently)

**ORCA overview:** Each actor manages its own heap. Memory is reclaimed via:
1. **Reference counting:** Most objects are freed immediately when their refcount hits zero
2. **Cycle detection:** Reference cycles are detected via a lightweight local cycle collector
3. **Cross-actor references:** When actor A holds a reference to an object in actor B, actor B tracks it as a "foreign reference." When the foreign reference is dropped, a decrement message is sent to actor B.

**Heap layout per actor:**

```
+------------------------------------------------------------------+
|                    Actor Linear Memory (WASM)                     |
|  +----------------------+-------------------------------------+   |
|  | Immutable Heap       | Mutable Heap                        |   |
|  | (actor code, static  | (state, message buffers, temp       |   |
|  |  data)               |  allocations)                       |   |
|  +----------------------+-------------------------------------+   |
|  | GC-managed objects   | ORCA allocator: bump + free list    |   |
|  +----------------------+-------------------------------------+   |
|  Max size: configurable (default 100MB, hard max 1GB)           |
+------------------------------------------------------------------+
```

**GC guarantees:**
- Pause time: < 1ms per collection (per-actor, no global pause)
- Collection frequency: triggered when heap crosses threshold (default: 75% of max)
- Cross-actor decrements: batched and sent asynchronously (not immediate)
- Memory limit enforcement: hard limit enforced by WASM memory bounds

### 3.6 Location Directory

The location directory is a distributed hash table mapping actor identities to their hosting nodes.

```
+------------------------------------------------------------------+
|                    Location Directory (per node)                  |
|                                                                   |
|  Actor Identity          ->  Node ID         | Status            |
|  -------------------------   --------------   | --------          |
|  Order:ord-42              ->  node-7        | active            |
|  User:sess-abc             ->  node-3        | active            |
|  Cart:cart-99              ->  node-7        | active            |
|  Payment:pay-12            ->  node-1        | deactivated       |
+------------------------------------------------------------------+
```

The directory is sharded by actor identity across all nodes via consistent hashing. Each node is responsible for a portion of the directory. Directory entries are cached locally with a TTL (default: 30 seconds). Stale cache entries are refreshed on message send failure.

---

## 4. Layer 3: Durable Execution

Durable execution is the persistence layer. It ensures that actor state survives crashes, restarts, and node failures. Without this layer, Nulang is an in-memory actor framework. With it, Nulang is a distributed durable execution platform.

### 4.1 Four State Models with Decision Flowchart

Every actor has a state model. The state model determines persistence, recovery, and consistency semantics. It is part of the actor's type — it cannot be changed after deployment.

```
                              START
                                |
                    +-----------+-----------+
                    |                       |
              Stateless?              Stateful?
                    |                       |
                    v                       v
              +---------+          +--------+--------+
              | `local` |          |  Needs audit?   |
              +---------+          +--------+--------+
                                         |
                              +----------+----------+
                              |                     |
                            No audit            Full audit
                              |                     |
                              v                     v
                       +-----------+       +----------------+
                       | `durable` |       | `event_sourced`|
                       +-----------+       +----------------+
                              |
                    Geo-replicated?
                              |
                    +---------+---------+
                    |                   |
                  Single           Multi-region
                  region               |
                    |                   v
                    v            +------------+
              (done)             |  `crdt`    |
                                 +------------+
```

**State model comparison:**

| Property | `local` | `durable` | `event_sourced` | `crdt` |
|----------|---------|-----------|-----------------|--------|
| Persistence | None | Automatic checkpoint | Event journal | CRDT merge |
| Recovery | None | State restore | Event replay | State sync |
| Consistency | N/A | Strong (single node) | Strong (single node) | Eventual |
| Multi-region | N/A | Active-passive | Active-passive | Active-active |
| Audit trail | No | No (checkpoints only) | Yes (full history) | Yes (ops only) |
| Overhead | Zero | ~1ms/checkpoint | Journal append | Merge on sync |
| Determinism | Not required | Recommended | Required | Required |

**Decision guide:**
- Use `local` for stateless computation, caches, protocol adapters
- Use `durable` as default for business logic (simplest, safest)
- Use `event_sourced` when you need audit history or deterministic replay
- Use `crdt` for geo-replicated collaborative state

### 4.2 Checkpointing Algorithm

**When to checkpoint:**

| Trigger | Default | Configurable | Rationale |
|---------|---------|--------------|-----------|
| Every message | Default for `durable` | Yes | Maximum safety |
| Every N messages | N=10 (configurable) | N | Amortize write cost |
| Every T seconds | Disabled by default | T | Time-based for batch |
| Memory threshold | 100MB | M | Prevent OOM |
| Before deactivation | Always | No | Ensure clean shutdown |

**What to checkpoint:**

```
Checkpoint payload:
+------------------+---------+-----------------------------------------+
| Field            | Size    | Description                             |
+------------------+---------+-----------------------------------------+
| Actor identity   | var     | Type + key                              |
| Sequence number  | 8B      | Monotonic message counter               |
| State blob       | var     | Serialized linear memory (LZ4 compressed)|
| Effect results   | var     | Map of (effect_id -> result) since last |
|                  |         | checkpoint                              |
| Mailbox snapshot | var     | Unprocessed messages (for exactly-once) |
| Timestamp        | 8B      | Wall clock (for debugging)              |
| Checksum         | 8B      | xxHash64 of entire payload              |
+------------------+---------+-----------------------------------------+
```

**How to checkpoint (async, non-blocking):**

```
1. Actor finishes message M_n
2. Scheduler marks actor as "checkpointing" (still in activation table)
3. Fork-on-write: actor's memory pages marked copy-on-write
4. Actor continues processing next message M_{n+1}
5. Background thread serializes checkpoint from COW snapshot
6. Serialized checkpoint written to storage backend (append-only)
7. On success: checkpoint metadata updated, COW pages released
8. On failure: actor marked for restart, supervisor handles recovery
```

The copy-on-write technique ensures that checkpointing does not block message processing. The actor incurs at most one page fault per written page during checkpointing. This is the same technique used by Erlang's `erlang:hibernate/3` and by Linux's fork() system call.

**Storage backends:**

| Backend | Use Case | Latency | Throughput |
|---------|----------|---------|------------|
| SQLite (local file) | Development | <1ms | 10K writes/sec |
| PostgreSQL | Single-node production | 1-5ms | 50K writes/sec |
| S3-compatible | Multi-node production | 10-50ms | Unlimited |
| Local NVMe (ephemeral) | High-throughput temp | <0.1ms | 100K writes/sec |

### 4.3 Event Journal Format and Storage

Event-sourced actors use an append-only event journal as their source of truth.

**Journal entry format:**

```
+--------+----------+----------+-------+----------+---------+----------+
| Header | Sequence | Timestamp| Type  | Payload  | Effect  | Checksum |
| (16B)  | (8B)     | (8B)     | (var) | (var)    | Results | (8B)     |
|        |          |          |       |          | (var)   |          |
+--------+----------+----------+-------+----------+---------+----------+

Header: magic (4B), version (2B), flags (2B), payload_len (4B), type_len (4B)
Type: event type name (e.g., "OrderCreated", "PaymentProcessed")
Payload: Cap'n Proto serialized event data
Effect Results: optional map of effect_id -> captured result
```

**Journal storage layout:**

```
journals/
  <actor_type>/
    <shard>/
      <actor_identity>/
        journal-00000001.seg    (10,000 events)
        journal-00000002.seg    (10,000 events)
        journal-00000003.seg    (5,000 events, current)
        snapshot-000020000.bin  (state at sequence 20,000)
        snapshot-000030000.bin  (state at sequence 30,000)
        meta.json               (journal metadata)
```

Segments are immutable once closed. Only the current (latest) segment is written to. Segments are written sequentially and never modified after close, enabling efficient replication (only the latest segment needs sync).

**Segment file format:**

```
+----------+----------+----------+----------+-----+----------+
| Index    | Entry 1  | Entry 2  | Entry 3  | ... | Trailer  |
| (offset  |          |          |          |     | (max     |
|  table)  |          |          |          |     |  seq)    |
+----------+----------+----------+----------+-----+----------+

Index: array of (sequence_number -> file_offset) for random access
Trailer: max sequence number, checksum, timestamp
```

### 4.4 Replay Mechanism and Determinism Enforcement

Replay is the ability to reconstruct an actor's exact state by replaying its event journal. It is the foundation of crash recovery and debugging.

**Replay algorithm:**

```
function replay(actor_identity, from_seq=0, to_seq=INF):
  1. Load latest snapshot at or before from_seq
  2. state = snapshot.state
  3. For each event in journal from snapshot.seq+1 to to_seq:
     a. Load event + captured effect results
     b. Apply projection function (pure, deterministic)
     c. For effects: return captured results, do not re-invoke handlers
     d. Update state
  4. Return state
```

**Determinism enforcement:** Nulang enforces determinism at compile time and runtime:

| Source of non-determinism | Compile-time enforcement | Runtime capture |
|---------------------------|-------------------------|-----------------|
| Time (`Now`) | Effect, not primitive | Captured timestamp |
| Randomness (`Random`) | Effect, not primitive | Captured value |
| External IO | Must use effects | Captured result |
| Message ordering | FIFO mailbox guarantee | Inherent |
| Internal iteration | Ordered data structures | Inherent |

**What happens on non-determinism violation:** If an event-sourced actor produces different output during replay (detected by hash mismatch of the state blob), the runtime:
1. Logs the divergence with full context
2. Halts replay for that actor
3. Notifies the supervisor
4. Supervisor restarts the actor from the last known-good snapshot
5. Human operator is alerted

This is a safety mechanism. Divergence should never happen in correct code. If it does, it indicates a bug (e.g., using `DateTime.now()` instead of `effect Now`).

### 4.5 Snapshotting Strategy

Snapshots are checkpoints of an event-sourced actor's projected state. They enable fast recovery without replaying from event 0.

**Snapshot triggers:**

| Trigger | Default | Description |
|---------|---------|-------------|
| Event count | Every 1,000 events | Regular snapshots for bounded replay |
| Time | Every 60 seconds | Time-based for low-volume actors |
| Size | When state > 10MB | Prevent oversized snapshots |
| Manual | On demand | Pre-deployment, debugging |

**Snapshot format:**

```
+----------+----------+----------+----------+----------+
| Sequence | Version  | State    | Index    | Checksum |
| Number   | (schema) | Blob     | (for     |          |
| (8B)     | (4B)     | (var)    |  partial | (8B)     |
|          |          |          |  load)   |          |
+----------+----------+----------+----------+----------+
```

**Tiered storage:**

```
+--------------------------------------------------------------+
|  Hot snapshots (last 10)     -> Local NVMe SSD               |
|  Warm snapshots (10-100)     -> Network-attached SSD         |
|  Cold snapshots (archived)   -> S3-compatible object store   |
+--------------------------------------------------------------+
```

**Partial loading:** For large state snapshots ( > 100MB), Nulang supports partial loading. The snapshot index maps state keys to file offsets. Only accessed keys are loaded into memory. This is essential for actors with large state (e.g., a shopping cart actor holding 10,000 items).

---

## 5. Layer 4: Distributed Platform

The Distributed Platform enables multiple Nulang runtime nodes to form a cluster. It handles node discovery, actor placement, cross-node messaging, and state replication.

### 5.1 Cluster Membership: Gossip Protocol with SWIM Failure Detection

Nodes discover each other and maintain a consistent view of cluster membership using a gossip protocol.

**SWIM protocol (Scalable Weakly-consistent Infection-style Process Group Membership):**

```
+--------------------------------------------------------------+
|  Node A                        Node B          Node C         |
|                                                              |
|  1. A picks random member (B)                               |
|     sends PING to B                                         |
|                                                              |
|  2. B receives PING, responds ACK                           |
|                                                              |
|  3. If A doesn't receive ACK within timeout:                |
|     A asks K random members (C, D) to PING B indirectly     |
|                                                              |
|  4. If no indirect ACK: B is marked failed                  |
|     Failure is gossiped to all members                      |
+--------------------------------------------------------------+
```

**Gossip dissemination:** In addition to failure detection, the gossip protocol disseminates:
- Node join/leave events
- Actor placement changes
- Configuration updates
- CRDT state deltas (piggybacked on gossip messages)

**Gossip message format:**

```
+----------+----------+----------+----------+----------+
| Sender   | Sequence | Updates  | Piggyback| Checksum |
| Node ID  | Number   | (array)  | (CRDT    |          |
|          |          |          |  deltas) |          |
+----------+----------+----------+----------+----------+

Updates: array of (type, payload) where type is:
  - NodeJoined { node_id, address, capabilities }
  - NodeFailed { node_id, timestamp }
  - ActorPlaced { actor_id, node_id }
  - ActorMigrated { actor_id, from_node, to_node }
  - ConfigChanged { key, value, version }
```

**Failure detection parameters:**

| Parameter | Default | Description |
|-----------|---------|-------------|
| Probe interval | 1 second | Time between direct probes |
| Probe timeout | 3 seconds | Time to wait for direct ACK |
| Indirect probes | 3 | Number of indirect probes on direct failure |
| Indirect timeout | 5 seconds | Time to wait for indirect ACK |
| Suspicion threshold | 3 | Number of missed probes before declaring failure |
| Sync interval | 200ms | Gossip sync frequency |

**False positive handling:** SWIM can falsely detect failure under high packet loss. Nulang mitigates this:
1. A node marked failed enters "suspected" state first
2. If suspected node responds to any probe, it is reinstated
3. If no response after suspicion threshold, node is declared failed
4. Actor placements on failed node are rebalanced to surviving nodes
5. If the "failed" node was actually alive (network partition), it detects the partition and shuts down (split-brain prevention)

### 5.2 Message Routing: Local vs Remote, Caching

Messages are routed by actor identity. The routing process determines whether the target is local or remote and delivers accordingly.

**Routing algorithm:**

```
function route_message(msg, target_identity):
  1. Hash target_identity -> consistent_hash_ring_position
  2. Look up responsible node in consistent hash ring
  3. If responsible node == this node:
       a. Look up target_identity in local activation table
       b. If active: push to mailbox
       c. If inactive: activate, then push to mailbox
  4. If responsible node == other node:
       a. Check local cache for target location
       b. If cache hit and fresh: forward to cached node
       c. If cache miss or stale: forward to responsible node
  5. If responsible node is failed:
       a. Find next replica in hash ring
       b. Forward to replica
       c. Trigger rebalancing for failed node's actors
```

**Consistent hash ring:**

```
Consistent Hash Ring (0 to 2^160 - 1, SHA-1 space)
+------------------------------------------------------------------+
|                                                                   |
|  node-1        node-2        node-3        node-1(replica)       |
|     |            |            |               |                  |
|     v            v            v               v                  |
|  +------+    +------+    +------+        +------+               |
|  |######|    |      |    |######|        |      |               |
|  |######|    |      |    |######|        |      |               |
|  +------+    +------+    +------+        +------+               |
|  0x10..    0x50..    0x90..         0xD0..                      |
|                                                                   |
|  Actor "Order:42" -> hash("Order:42") = 0x67 -> node-2          |
|  Actor "Cart:99"  -> hash("Cart:99")  = 0xB3 -> node-3          |
+------------------------------------------------------------------+
```

Each node is placed at multiple points on the ring (virtual nodes, default: 150 per physical node). This ensures uniform distribution even with non-uniform actor identity hashing.

**Location cache:** Each node maintains an LRU cache of actor identity -> node mappings. Cache entries have a TTL (default: 30 seconds). A cache hit avoids a consistent hash computation and a directory lookup. On cache miss or stale entry, the router falls back to the directory.

### 5.3 CRDT Replication: Anti-Entropy Protocol

CRDT actors replicate state across nodes without coordination. The replication uses an anti-entropy protocol.

**Anti-entropy process:**

```
Node A (CRDT replica)              Node B (CRDT replica)
       |                                    |
       | 1. A initiates sync                |
       |    sends sync request with         |
       |    vector clock + digest           |
       |----------------------------------->|
       |                                    |
       | 2. B compares digest with          |
       |    its own state                   |
       |    identifies missing deltas       |
       |<-----------------------------------|
       |    sends delta batch               |
       |                                    |
       | 3. A applies deltas, sends         |
       |    ack + any deltas B is missing   |
       |----------------------------------->|
       |                                    |
       | 4. B applies A's deltas            |
       |<-----------------------------------|
       |    final ack                       |
```

**CRDT types supported:**

| Type | Operations | Merge Semantics | Use Case |
|------|-----------|-----------------|----------|
| G-Counter | increment | max of replicas | Vote counting, page views |
| PN-Counter | increment, decrement | per-replica G-Counters | Like/dislike counts |
| G-Set | add | set union | Tag collections |
| 2P-Set | add, remove | add-wins over remove | Shopping cart items |
| OR-Set | add, remove | add-wins, unique tags | Collaborative lists |
| LWW-Register | set | last-writer-wins | User preferences |
| MV-Register | set | multi-value on conflict | Concurrent edits |
| OR-Map | put, remove, nested CRDTs | recursive merge | Document stores |
| Delta-State | all above | delta-based sync | Efficient replication |

**Sync modes:**

| Mode | Trigger | Latency | Bandwidth |
|------|---------|---------|-----------|
| Continuous | Every update | Low | High |
| Periodic | Every 5 seconds | Medium | Low |
| On-demand | Pull-based | High | Lowest |
| Gossip piggyback | On gossip messages | Medium | Very low |

The default is gossip piggyback for low-throughput CRDTs and periodic (1s) for high-throughput CRDTs.

### 5.4 Service Discovery

Actor protocols are registered in a distributed service registry. Clients discover actors by protocol type, not by hardcoded identity.

**Registry structure:**

```
+------------------------------------------------------------------+
|                     Service Registry                              |
|                                                                   |
|  Protocol Type       ->  Actor Type(s)       | Instances         |
|  -----------------      -----------------     | --------          |
|  OrderService        ->  OrderActor          | [ord-42, ord-43]  |
|  PaymentGateway      ->  StripeGateway       | [stripe]          |
|                       ->  PayPalGateway      | [paypal]          |
|  UserSession         ->  SessionActor        | [sess-abc, ...]   |
+------------------------------------------------------------------+
```

**Discovery patterns:**

| Pattern | API | Use Case |
|---------|-----|----------|
| By identity | `actor("Order:ord-42")` | Known entity (specific order) |
| By type | `actor_of_type(OrderService)` | Any instance (round-robin) |
| By key range | `actors_in_range("User:", start, end)` | Shard scanning |
| By capability | `actors_with(LLM)` | Find all LLM-enabled actors |

The registry is itself a CRDT, replicated across all nodes via gossip. Registration is implicit: when an actor type is deployed, it is automatically registered. Deregistration happens on deployment removal.

### 5.5 Network Transport: NUL0 Protocol

NUL0 is Nulang's inter-node network protocol. It is a binary protocol over TCP (with optional QUIC for cross-region).

**NUL0 frame format:**

```
+--------+--------+--------+--------+--------+----------+----------+
| Magic  | Version| Type   | Flags  | Length | Payload  | MAC      |
| (4B)   | (1B)   | (1B)   | (2B)   | (4B)   | (var)    | (16B)    |
+--------+--------+--------+--------+--------+----------+----------+

Magic: 0x4E554C30 ("NUL0" in ASCII)
Version: 0x01 (current)
Type: Message, Ack, Nack, Heartbeat, Gossip, CRDTSync, Control
Flags: encrypted, compressed, priority
MAC: Poly1305 message authentication code
```

**Connection management:**

```
+------------------------------------------------------------------+
|                    Connection Pool (per node pair)                |
|                                                                   |
|  Node A maintains persistent TCP connections to all other nodes   |
|  Connection count per pair: min(4, CPU cores)                     |
|                                                                   |
|  +----------+    +----------+    +----------+    +----------+   |
|  | Conn 1   |    | Conn 2   |    | Conn 3   |    | Conn 4   |   |
|  | (msgs)   |    | (gossip) |    | (crdt)   |    | (ctrl)   |   |
|  +----------+    +----------+    +----------+    +----------+   |
|                                                                   |
|  Separation: different traffic types use different connections    |
|  to prevent head-of-line blocking                                 |
+------------------------------------------------------------------+
```

**Security:**
- All inter-node traffic is encrypted (TLS 1.3 over TCP, or native QUIC encryption)
- Messages are authenticated with Poly1305 MACs
- Capability tokens are embedded in message headers and verified at the receiving node
- Node identities are verified via Ed25519 signatures exchanged during handshake

---

## 6. Layer 5: AI Runtime

The AI Runtime (v0.9) is split across two crates. Pure types live in the
standalone `nulang-ai` workspace crate (`crates/nulang-ai/`, published as
`nulang_ai`) with **zero dependency on the core `nulang` crate**. The core
crate imports them directly (`use nulang_ai::…;`, no façade module) behind
the `ai-runtime` cargo feature and adds the language-level wiring for
`agent` declarations and orchestration builtins plus the actor-runtime
integration that legitimately needs `Runtime` internals. It is not a
separate interpreter: agents compile to ordinary persistent actors, and LLM
calls flow through the same register VM and callback traits as every other
effect. There is no WIT layer, no capability-injected provider, and no
routing DSL in the current implementation — provider selection is a
Rust-level decision (`Runtime::set_llm_client`).

### 6.1 Module Layout

| File | Contents |
|------|----------|
| `crates/nulang-ai/src/mod.rs` | Crate root; declares modules and re-exports the public surface |
| `crates/nulang-ai/src/client.rs` | `LlmClient` async trait + `complete_sync` bridge |
| `crates/nulang-ai/src/request.rs` | `LlmRequest`, `LlmMessage`, `ToolSchema`, `ModelPricing` |
| `crates/nulang-ai/src/response.rs` | `LlmResponse`, `ToolCall`, `TokenUsage`, `LlmError` |
| `crates/nulang-ai/src/providers/openai.rs` | `OpenAiClient` (OpenAI + compatible endpoints) |
| `crates/nulang-ai/src/providers/ollama.rs` | `OllamaClient` (local Ollama) |
| `crates/nulang-ai/src/mock.rs` | `MockLlmClient` (canned/sequenced responses, records requests) |
| `crates/nulang-ai/src/memory.rs` | `EpisodicMemory` — bounded conversation buffer |
| `crates/nulang-ai/src/semantic_memory.rs` | `SemanticMemory` — cosine-similarity fact store |
| `crates/nulang-ai/src/procedural_memory.rs` | `ProceduralMemory` — patterns + few-shot examples |
| `crates/nulang-ai/src/usage.rs` | `estimated_cost`, `TokenBudget`, `UsageSummary` accumulation |
| `crates/nulang-ai/src/pipeline.rs` | `Pipeline` / `PipelineStage` + `PipelineRuntime` trait |
| `crates/nulang-ai/src/supervisor.rs` | `SupervisorTeam` / `Worker` + `SupervisorRuntime` trait |
| `crates/nulang-ai/src/debate.rs` | `Debate` / `Participant` / `Stance` + `DebateRuntime` trait |
| `crates/nulang-ai/src/registry.rs` | `AiRuntimeRegistry` (pipelines + debates) and `SupervisorTeamRegistry`, both generic over the runtime traits |
| `src/runtime/ai_impls.rs` | Core-side `impl PipelineRuntime for Runtime`, `impl DebateRuntime for Runtime`, `impl SupervisorRuntime for Runtime`. Kept in core by the orphan rule |
| `src/runtime/agent.rs` | Agent LLM completion pipeline (`build_agent_llm_request`, `finish_agent_llm`, `complete_agent_llm`) — reads actor durable state, allocates strings into actor heaps |
| `src/runtime/llm.rs` | `LlmState` — persistent `nulang-llm` worker thread + completion channels polled by the scheduler |
| `src/tool_schema.rs` | Nulang `Type` → JSON Schema, `function_to_tool_schema` — kept in core (unconditional) because `bytecode::ActorMeta.tools` and HIR both embed it |

### 6.2 Provider Abstraction: Async Trait, Sync Bridge

All providers implement one trait (`crates/nulang-ai/src/client.rs`):

```rust
#[async_trait]
pub trait LlmClient: Send + Sync {
    async fn complete(&self, request: LlmRequest) -> Result<LlmResponse, String>;
}
```

The runtime and VM are synchronous, so `complete_sync(client, request)`
bridges the gap: it reuses the current Tokio runtime handle when one exists
(`Handle::try_current().block_on(...)`), and otherwise builds a temporary
single-threaded Tokio runtime for the duration of the call. The only async
code in the project is this trait, `main` (`#[tokio::main]`), and the LSP
server.

`LlmRequest` carries `model`, `messages`, `tools`, `memory` (prepended to
`messages` by the provider before sending), and optional `pricing`
(`ModelPricing`, USD per 1k input/output tokens). `LlmResponse` carries
`content: Option<String>`, `tool_calls`, `model`, `finish_reason`, and
`TokenUsage { prompt, completion, total }`.

**Providers** (both `reqwest`-based, `stream: false` — no streaming today):

| Provider | Endpoint | Tool calling | Construction |
|----------|----------|--------------|--------------|
| `OpenAiClient` | `POST {base_url}/chat/completions`, `Authorization: Bearer` | Native function-calling format, `tool_choice: "auto"` | `new(key, model)`; `with_base_url(...)` for OpenAI-compatible APIs; `gpt4o()` reads `OPENAI_API_KEY` |
| `OllamaClient` | `POST {base_url}/api/chat` | Native | `new(base_url, model)`; `default()` = `http://localhost:11434`, `llama3.1` |

`MockLlmClient` returns a fixed response, a sequence of responses, or tool
calls, and records every request it receives — it is how agent, pipeline,
and debate tests run without a network.

There is no multi-provider routing and no budget auto-fallback yet; Anthropic,
Azure, and vLLM backends from the target design are unimplemented.

### 6.3 Agent Declarations Are Compiled, Not Interpreted

`agent` is a real declaration form wired through the whole pipeline — lexer
(`"agent"` keyword, `src/lexer.rs`) → parser (`parse_agent`,
`src/parser.rs`) → AST (`Decl::Agent`, `src/ast.rs`) → type checker (binds an
opaque synthetic `Actor` type) → HIR desugaring → MIR → bytecode metadata →
runtime support. It is not library-only sugar assembled in Rust code.

Syntax (`examples/pipeline.nula`):

```nulang
agent Researcher = {
    model: "llama3.1",
    system_prompt: "You are a researcher. Provide factual information.",
    pricing: { input: 0.0, output: 0.0 }
}
```

Recognized fields — anything else is a parse error: `model` (required
string), `system_prompt` (string), `tools` (list of function names),
`memory: { max_turns }` (default 50), `semantic_memory: { dimensions }`,
`procedural_memory: { namespace }`, `pricing: { input, output }` (USD per 1k
tokens).

`hir_lower::desugar_agent` rewrites the declaration into a **persistent
actor** (`is_agent: true`) whose durable state holds the configuration and
whose generated behaviors provide the interface:

- State fields (all `StateModel::Durable`): `model`, `system_prompt`,
  `episodic_memory` (an `EpisodicMemory` serialized as JSON),
  `usage_prompt`, `usage_completion`, `usage_cost`, `pricing_input`,
  `pricing_output`, and — when configured — `semantic_memory` and
  `procedural_memory` (JSON snapshots of the corresponding stores).
- `LLM.ask(prompt)`, which MIR lowering dispatches through the generic
  `PerformAsync` (0xC6) effect opcode with the `"Inference.ask"` effect-op
  string.

At the VM boundary (`src/vm.rs`), `PerformAsync` with `"Inference.ask"` reads
the model string from the constant pool and the prompt from a register, calls
`ActorVmCallbacks::complete_llm(model, prompt)`, and writes the reply string
(or `nil`) back into the same register. The bytecode module records agent
metadata (`ActorMeta { is_agent, tools, semantic_memory_dimensions,
procedural_memory_namespace }`) so the runtime knows which actors are
agents.

On the runtime side (`src/runtime/mod.rs`):

- `Runtime::set_llm_client(Box<dyn LlmClient>)` installs the provider. With
  no client installed, LLM requests fail with an error string that becomes
  `nil` at the VM boundary — agents degrade gracefully instead of panicking.
- `complete_agent_llm(actor_id, prompt)` loads the agent's durable state,
  builds the request (system prompt + episodic memory + user prompt), calls
  `complete_llm_with_tools`, then writes the updated episodic memory back and
  accumulates token usage and cost into durable state — so memory and spend
  survive actor restarts.
- `complete_llm_with_tools` populates `request.tools` from `module.tools`;
  if the model returns tool calls, each named function is invoked with the
  JSON arguments (`invoke_agent_tool_function`) and the results are appended
  to the conversation for a final model round-trip.

### 6.4 Memory: Three Kinds

The design's "3-tier memory" exists today as three Rust types, all
serde-serializable so they can live in durable actor state:

| Kind | Type | Scope | Mechanism |
|------|------|-------|-----------|
| Episodic | `EpisodicMemory` (`crates/nulang-ai/src/memory.rs`) | Conversation | `VecDeque<Turn>` bounded by `max_turns` (default 50); oldest evicted first; `to_messages()` materializes provider-agnostic `LlmMessage`s |
| Semantic | `SemanticMemory` (`crates/nulang-ai/src/semantic_memory.rs`) | Long-term facts | `store` / `search(query, top_k)` / `delete`; cosine similarity over embeddings — a deterministic built-in FNV-based embedder by default, or a caller-supplied `fn(&str) -> Vec<f32>`; brute-force scan |
| Procedural | `ProceduralMemory` (`crates/nulang-ai/src/procedural_memory.rs`) | Learned patterns | Namespaced `Pattern`s (`input_pattern` → `output_template`) plus few-shot `Example`s per task; `get_examples` ranks by keyword overlap, falling back to most-recent |

Agent declarations opt into semantic/procedural memory via
`semantic_memory: { dimensions }` and `procedural_memory: { namespace }`;
the generated actor keeps each store as a JSON blob in durable state and the
runtime services the memory behaviors by name (§6.3).

Not implemented yet: context-window auto-summarization, external vector
backends (Qdrant/pgvector/HNSW), and the journal-backed "event memory" tier.

### 6.5 Tools: `@tool` Functions, Not a Separate Registry

A tool is an ordinary function annotated `@tool(description: "...")`
(`FunctionAnnotation::Tool`, parsed in `src/parser.rs`). There is no
standalone tool-registry process and no separate definition format:

1. `ai::schema::function_to_tool_schema` builds a `ToolSchema`
   (`{ name, description, parameters }`) from the function signature;
   `type_to_json_schema` maps Nulang types to the JSON-Schema subset —
   primitives, records → objects, arrays, tuples → `prefixItems`, variants →
   `oneOf`, references unwrap, `Option[T]` → `anyOf [null, T]`.
2. Codegen collects the schemas of every agent actor into `CodeModule.tools`
   (`src/mir_codegen.rs`).
3. At request time the runtime attaches them to the outgoing request; the
   provider returns `tool_calls`; the runtime invokes each named function
   with the JSON arguments and feeds the results back for a final model
   round-trip (§6.3).

### 6.6 Orchestration: Pipelines, Supervisor Teams, Debates

Three multi-agent patterns ship both as Rust types and as surface-syntax
builtins. Each pattern defines a minimal runtime trait —
`PipelineRuntime`, `SupervisorRuntime`, `DebateRuntime`, each with a single
`ask_agent(agent_id, prompt)` method — so tests can drive them with mocks.
All three are implemented for `Runtime` by `ask`-ing the target agent actor
(the `ask` behavior is resolved by name, with a bytecode-module fallback for
source-compiled agents).

| Pattern | Type | Semantics |
|---------|------|-----------|
| Pipeline | `Pipeline { stages: Vec<PipelineStage> }` | Sequential chain of `PipelineStage { name, agent_id, prompt_template }`; `{input}` in each template is replaced by the previous stage's output (or the pipeline input for the first stage) |
| Supervisor team | `SupervisorTeam { workers, max_iterations }` | Sequential refinement over `Worker { name, agent_id, description }`; each worker is prompted with its description plus the accumulated result |
| Debate | `Debate { topic, rounds, consensus_threshold, participants }` | `rounds` rounds over `Participant { name, stance: Pro/Con/Neutral, agent_id }` with the running argument record in each prompt; the **last** participant acts as moderator and synthesizes the conclusion (`consensus_threshold` is clamped to [0,1] and currently advisory) |

Surface syntax such as

```nulang
let pipeline = Pipeline.new()
    |> Pipeline.stage("research", researcher, "Research: {input}")
    |> Pipeline.stage("write", writer, "Write based on: {input}")
in
pipeline.run("CRDTs")
```

(`examples/pipeline.nula`) is recognized in HIR lowering
(`is_ai_builtin_call` / `try_lower_run_call`, `src/hir_lower.rs`) and
compiled to dedicated opcodes — `PipelineNew/Stage/Run`,
`SupervisorNew/Worker/Run`, `DebateNew/Participant/Run` (§2.6). The VM
forwards them to the runtime, which owns `pipelines`, `supervisor_teams`,
and `debates` maps keyed by monotonic ids.

The target design's LLM-driven planner (natural-language goal → workflow
graph) is not implemented; `workflow` declarations (§2.1) are the only
workflow mechanism today.

### 6.7 Usage Tracking and Cost

`TokenUsage` (per response) feeds `estimated_cost(usage, pricing)`
(`crates/nulang-ai/src/usage.rs`) with per-1k-token USD rates from `ModelPricing` (or the
agent's `pricing` block). `UsageSummary` accumulates prompt/completion/total
tokens (saturating adds) and cost (plain `f64` sum).

For agent actors this accumulation is automatic: `complete_agent_llm` writes
the running totals into durable state after every call, and the generated
`usage()` behavior exposes `[prompt, completion, cost]`.

Not implemented yet: OpenTelemetry span emission, PII redaction, latency
tracing, per-deployment cost counters, and budget enforcement or alerts —
the observability half of the target design remains open.

---

## 7. Data Flow Diagrams

### 7.1 Local Message Flow

```
+--------+     +---------+     +----------+     +----------+     +-------+
| Sender |     | Message |     | Scheduler|     | Behavior |     |Mailbox|
| Actor  |     | Router  |     | (work    |     | Handler  |     | (next)|
|        |     |         |     |  queue)  |     | (WASM)   |     |       |
+---+----+     +----+----+     +----+-----+     +----+-----+     +---+---+"
    |               |                |                |                |
    | 1. send(msg)  |                |                |                |
    |-------------->|                |                |                |
    |               | 2. route to    |                |                |
    |               |    scheduler   |                |                |
    |               |--------------->|                |                |
    |               |                | 3. enqueue     |                |
    |               |                |    in run queue|                |
    |               |                |                |                |
    |               |                | 4. schedule    |                |
    |               |                |    actor       |                |
    |               |                |--------------->|                |
    |               |                |                | 5. dequeue msg |
    |               |                |                |                |
    |               |                |                | 6. run handler |
    |               |                |                |    in WASM     |
    |               |                |                |                |
    |               |                |                | 7. handler     |
    |               |                |                |    completes   |
    |               |                |                |                |
    |               |                | 8. deliver     |                |
    |               |                |    outgoing    |                |
    |               |                |    messages    |                |
    |               |                |                |                |
    |               |                | 9. if reply    |                |
    |<------------------------------------expected:   |                |
    |               |                |    send reply  |                |
    |               |                |                |                |
    |               |                | 10. requeue    |                |
    |               |                |    if more msgs|---------------->|
    +               +                +                +                +
```

### 7.2 Durable Execution Flow

```
+---------+     +----------+     +-----------+     +----------+     +--------+
|Behavior |     |Checkpoint|     |  Storage  |     | Recovery |     |Replay  |
|Handler  |     | Manager  |     |  Backend  |     |Orchestr. |     |Engine  |
+----+----+     +----+-----+     +-----+-----+     +----+-----+     +---+----+
     |               |                  |                |               |
     | 1. handler    |                  |                |               |
     |    completes  |                  |                |               |
     |-------------->|                  |                |               |
     |               | 2. COW snapshot  |                |               |
     |               |    of state      |                |               |
     |    (actor     |                  |                |               |
     |     continues)|                  |                |               |
     |               | 3. serialize +   |                |               |
     |               |    compress      |                |               |
     |               |----------------->|                |               |
     |               |                  | 4. append to   |                |
     |               |                  |    journal     |                |
     |               |<-----------------|                |               |
     |               | 5. ack           |                |               |
     |               |                  |                |               |
     |               |                  |                |               |
     |               |                  |                | 6. node fails  |
     |               |                  |                |               |
     |               |                  |                | 7. detect      |
     |               |                  |                |    failure     |
     |               |                  |<---------------|               |
     |               |                  | 8. load latest |                |
     |               |                  |    checkpoint  |                |
     |               |                  |                |               |
     |               |                  |                | 9. replay      |
     |               |                  |                |    events from |
     |               |                  |                |    checkpoint  |
     |               |<-----------------------------------|                |
     |               | 10. restore state                  |                |
     |<--------------|                                    |                |
     | 11. resume    |                                    |                |
     |    processing |                                    |                |
     +               +                  +                 +                +

For event_sourced actors, step 8 loads the snapshot and steps 9-10 replay
events after the snapshot. Effect results from the journal are used instead
of re-invoking handlers.
```

### 7.3 Distributed Message Flow

```
+--------+     +----------+     +----------+     +----------+     +--------+
|Local   |     |Location  |     | NUL0     |     |Location  |     |Remote  |
|Actor   |     |Directory |     |Transport |     |Directory |     |Actor   |
+---+----+     +----+-----+     +-----+----+     +-----+----+     +---+----+
    |               |                  |                |               |
    | 1. send(msg,  |                  |                |               |
    |    "Order:42")|                  |                |               |
    |-------------->|                  |                |               |
    |               | 2. hash("Order:42")|                |               |
    |               |    -> node-3       |                |               |
    |               |                  |                |               |
    |               | 3. if local:     |                |               |
    |               |    deliver       |                |               |
    |               |    if remote:    |                |               |
    |               |    forward       |                |               |
    |               |----------------->|                |               |
    |               |                  | 4. serialize   |                |
    |               |                  |    + encrypt   |                |
    |               |                  |                |               |
    |               |                  | 5. TCP send    |                |
    |               |                  |--------------->|               |
    |               |                  |                | 6. receive     |
    |               |                  |                |    + decrypt   |
    |               |                  |                |               |
    |               |                  |                | 7. look up     |
    |               |                  |                |    activation  |
    |               |                  |                |               |
    |               |                  |                | 8. deliver to  |
    |               |                  |                |    mailbox     |
    |               |                  |                |-------------->|
    |               |                  |                | 9. send ACK   |
    |               |                  |<---------------|               |
    |               |                  | 10. ACK        |               |
    |               |<-----------------|                |               |
    | 11. delivery   |                  |                |               |
    |     confirmed |                  |                |               |
    +               +                  +                +               +
```

### 7.4 CRDT Sync Flow

```
+--------+     +---------+     +----------+     +---------+     +--------+
|Local   |     |CRDT     |     | Gossip / |     |CRDT     |     |Remote  |
|Modify  |     |Engine    |     | NUL0     |     |Engine   |     |Merge   |
+---+----+     +----+----+     +-----+----+     +----+----+     +---+----+
    |               |                  |                |               |
    | 1. apply(op)  |                  |                |               |
    |-------------->|                  |                |               |
    |               | 2. update local  |                |               |
    |               |    state         |                |               |
    |               |                  |                |               |
    | 3. return OK  |                  |                |               |
    |<--------------|                  |                |               |
    |               |                  |                |               |
    |               | 4. delta =       |                |               |
    |               |    state.delta() |                |               |
    |               |                  |                |               |
    |               | 5. encode delta  |                |               |
    |               |----------------->|                |               |
    |               |                  | 6. send via    |                |
    |               |                  |    gossip/NUL0 |                |
    |               |                  |--------------->|                |
    |               |                  |                | 7. receive     |
    |               |                  |                |    delta       |
    |               |                  |                |               |
    |               |                  |                | 8. merge(delta)|
    |               |                  |                |    (converged) |
    |               |                  |                |               |
    |               |                  |                | 9. if local    |
    |               |                  |                |    state       |
    |               |                  |                |    changed:    |
    |               |                  |                |    notify      |
    |               |                  |                |    actor       |
    |               |                  |                +------->|       |
    +               +                  +                +        +       +

No coordination, no locks, no consensus. Both replicas apply the operation
locally and converge via the CRDT merge function.
```

### 7.5 AI Workflow Flow

```
+--------+     +--------+     +----------+     +--------+     +--------+
| User   |     | Agent  |     | AI       |     | Tool   |     | LLM    |
| Message|     | Actor  |     | Runtime  |     | Exec   |     |Provider|
+---+----+     +---+----+     +-----+----+     +---+----+     +---+----+
    |              |                |                |               |
    | 1. message   |                |                |               |
    |------------->|                |                |               |
    |              | 2. build       |                |               |
    |              |    conversation|                |               |
    |              |    (short-term  |                |               |
    |              |     memory)     |                |               |
    |              |                |                |               |
    |              | 3. recall      |                |               |
    |              |    long-term    |                |               |
    |              |    memory       |                |               |
    |              |                |                |               |
    |              | 4. effect      |                |               |
    |              |    LLMComplete  |                |               |
    |              |--------------->|                |               |
    |              |                | 5. send to     |                |
    |              |                |    LLM provider|                |
    |              |                |--------------->|                |
    |              |                |                | 6. LLM decides |
    |              |                |                |    tool call   |
    |              |                |<---------------|                |
    |              |                | 7. parse tool  |                |
    |              |                |    call        |                |
    |              |                |                |               |
    |              |                | 8. send to     |                |
    |              |                |    tool actor  |                |
    |              |                |--------------->|                |
    |              |                |                | 9. execute     |
    |              |                |                |    behavior    |
    |              |                |                |               |
    |              |                | 10. tool result|                |
    |              |                |<---------------|                |
    |              |                |                |               |
    |              |                | 11. re-send to |                |
    |              |                |    LLM with    |                |
    |              |                |    tool result |                |
    |              |                |--------------->|                |
    |              |                |                | 12. final      |
    |              |                |                |    response    |
    |              |                |<---------------|                |
    |              |                |                |               |
    |              | 13. emit trace |                |               |
    |              |    (async)     |                |               |
    |              |                |                |               |
    | 14. response |                |                |               |
    |<-------------|                |                |               |
    +              +                +                +               +
```

---

## 8. Component Interaction Map

| Component | Talks To | Protocol | Purpose |
|-----------|----------|----------|---------|
| **Scheduler** | Actor (via mailbox) | Mailbox push (lock-free) | Work distribution |
| **Scheduler** | Location Directory | Read (local cache) | Determine if actor is local |
| **Scheduler** | Checkpoint Manager | Async callback | Trigger post-message checkpoint |
| **Virtual Actor Manager** | Location Directory | Read/Write | Track activations |
| **Virtual Actor Manager** | WASM Sandbox Pool | Acquire/Release | Get WASM instance for activation |
| **Mailbox** | Scheduler | Dequeue (consumer) | Deliver messages to scheduled actor |
| **Mailbox** | Message Router | Enqueue (producer) | Accept incoming messages |
| **Message Router** | Location Directory | Lookup | Resolve actor identity to node |
| **Message Router** | NUL0 Transport | Send frame | Forward remote messages |
| **Message Router** | Local Scheduler | Enqueue | Deliver local messages |
| **Supervisor** | Child actors | EXIT signals, restart commands | Fault recovery |
| **Supervisor** | Parent supervisor | Escalation | Propagate unrecoverable failures |
| **WASM Sandbox Pool** | Wasmtime | Instantiate/Call host | Run actor code |
| **Checkpoint Manager** | Storage Backend | Append | Write checkpoint blobs |
| **Checkpoint Manager** | Actor (COW) | Read memory | Capture actor state |
| **Event Journal** | Storage Backend | Append segment | Persist event log |
| **Event Journal** | Replay Engine | Read segment | Replay events for recovery |
| **Replay Engine** | Actor (WASM) | Inject effect results | Deterministic replay |
| **Recovery Orchestrator** | Event Journal | Read | Load actor state after crash |
| **Recovery Orchestrator** | Scheduler | Activate | Resume recovered actors |
| **Gossip Protocol** | All nodes | UDP multicast / TCP | Membership, failure detection |
| **Gossip Protocol** | Location Directory | Push updates | Propagate placement changes |
| **Gossip Protocol** | CRDT Engine | Piggyback deltas | Efficient CRDT sync |
| **CRDT Engine** | Actor (local) | Apply ops | Local CRDT updates |
| **CRDT Engine** | NUL0 Transport | Send deltas | Cross-node CRDT sync |
| **CRDT Engine** | Storage Backend | Read/Write | Persist CRDT state |
| **Location Directory** | Consistent Hash Ring | Compute | Map identity to node |
| **Location Directory** | Service Registry | Read/Write | Register actor types |
| **Service Registry** | Gossip Protocol | Replicate | Cross-node registry sync |
| **NUL0 Transport** | All nodes | TCP / QUIC | Encrypted inter-node messaging |
| **AI Runtime** | LLM Provider | HTTP/2 (OpenAI, Anthropic) | LLM completions |
| **AI Runtime** | Tool Registry | Read | Discover available tools |
| **AI Runtime** | Actor (via effect) | Effect return | Deliver LLM responses |
| **AI Runtime** | Vector Store | Query/Upsert | Long-term memory |
| **AI Runtime** | Event Journal | Read | Event memory queries |
| **AI Runtime** | Observability | Emit spans | LLM call traces |
| **Tool Registry** | Service Registry | Query | Map tool names to actors |
| **Tool Registry** | Scheduler | Send/Receive | Execute tool calls |
| **Capability Manager** | Actor (compile-time) | Type check | Verify capability usage |
| **Capability Manager** | Message Router | Verify tokens | Validate cross-node caps |
| **Capability Manager** | Revocation List | Append/Check | Lazy capability revocation |
| **ORCA GC** | Actor (per-actor) | Collect | Reclaim actor-local memory |
| **ORCA GC** | Cross-actor | Send decrements | Reclaim cross-actor references |

---

## 9. Architecture Decision Records

### ADR-001: Virtual Actors by Default

**Status:** Partially implemented — actors today have numeric `u64` identities and explicit `spawn`; identity-string addressing, activation-on-first-message, and hash-ring placement remain target design (see §1 implementation note).
**Context:** Actor lifecycle management is complex and error-prone. Erlang requires explicit spawn/link/monitor. Akka requires actor-of-actor factories. Both force developers to manage lifetimes, introducing coupling between creator and created.
**Decision:** All actors in Nulang are virtual — addressed by identity, runtime manages activation/deactivation/ placement/migration. Creating an actor is as simple as sending it a message.
**Consequences:** (+) Simpler programming model, location transparency, automatic scaling. (-) Requires distributed directory service, potential "cold start" latency for first message.
**References:** Orleans (Microsoft Research), "Orleans: Distributed Virtual Actors for Programmability and Scalability" (Bernstein et al., 2014)

### ADR-002: WASM as Compilation Target

**Status:** Superseded (2026-07) — the implementation compiles to register-VM bytecode with a Cranelift JIT tier; no WASM/Wasmtime exists in the tree. See §2.5–§2.6. The sandboxing/portability goals remain open.
**Context:** Nulang needs sandboxed execution, cross-language interop, and portable deployment. Options: native code (fast but unsafe), JVM (heavyweight, Oracle licensing), BEAM (Erlang-specific), WASM (sandboxed, portable, standard).
**Decision:** Nulang compiles to WASM components (wasm32-wasip2). Every actor is a WASM component running in Wasmtime.
**Consequences:** (+) Sandboxed execution, cross-language composition, portable deployment, capability-based security via WASI. (-) ~10-20% performance overhead vs native, dependency on Wasmtime project health.
**References:** WebAssembly Component Model W3C specification, Wasmtime runtime

### ADR-003: Four State Models

**Status:** Accepted
**Context:** Different use cases need different persistence/consistency tradeoffs. A single model (e.g., always event-sourced) is too expensive for simple stateless services. No persistence makes Nulang just another actor framework.
**Decision:** Nulang provides four state models: `local` (no persistence), `durable` (automatic checkpointing), `event_sourced` (full audit journal), `crdt` (geo-replicated). The state model is part of the actor's type.
**Consequences:** (+) Optimal cost/performance for each use case, type-safe persistence. (-) More complexity in the runtime, migration between state models requires code changes.
**References:** Orleans grain persistence, Durable Functions stateful entities, Akka Persistence, CRDT research (Shapiro et al.)

### ADR-004: No Separate AI DSL

**Status:** Accepted, with a 2026-07 refinement: `agent` is now a first-class declaration form, but it *desugars* to an ordinary persistent actor (§6.3) — the "agents are actors, no separate interpreter" principle holds. Tools are `@tool`-annotated functions (§6.5); memory is durable actor state (§6.4).
**Context:** AI frameworks (LangChain, LangGraph) create parallel programming models for agents. This forces developers to learn two systems and prevents agents from using general distributed systems features.
**Decision:** There is no separate AI DSL in Nulang. An AI agent is an actor that holds the `LLM` capability. Tools are actor behaviors. Memory is actor state. Planning delegates to the workflow subsystem.
**Consequences:** (+) Unified programming model, agents get durability/distribution/supervision for free, no framework lock-in. (-) AI-specific ergonomics (prompt templates, conversation management) must be provided as libraries, not language features.
**References:** LangGraph, AutoGen, "Actors Are All You Need" (design principle)

### ADR-005: Workflows as Actor Graphs

**Status:** Accepted
**Context:** Workflow engines (Temporal, Camunda) are separate systems that integrate poorly with actor models. Workflow steps are not actors — they cannot send/receive messages, hold state, or be supervised individually.
**Decision:** Workflow definitions compile to actor graphs. Each step is a child actor. The workflow actor is a durable state machine tracking step completion. Control flow (parallel, conditional, compensation) compiles to message patterns.
**Consequences:** (+) Workflows inherit all actor properties (durability, distribution, supervision), no separate workflow engine to operate, workflow steps can be any actor. (-) Workflow compilation is complex, workflow visualization requires mapping actor graphs back to workflow steps.
**References:** Temporal (workflow as deterministic program), Saga pattern, Erlang gen_fsm

### ADR-006: ORCA Garbage Collection

**Status:** Accepted
**Context:** Stop-the-world GC is incompatible with actor systems where each actor must process messages with predictable latency. Per-actor heaps need a collection strategy that doesn't pause other actors.
**Decision:** Nulang uses ORCA (Ownership and Reference Counting based on Cycles in Actors) for per-actor garbage collection. Each actor collects independently. Cross-actor references use asynchronous decrement messages.
**Consequences:** (+) <1ms pause per actor, no global pause, naturally parallel. (-) Reference counting overhead (~5-10%), cycle detection requires periodic local tracing, cross-actor decrements are eventual (not immediate).
**References:** "ORCA: A Causal Delta-Order for Concurrent Reclamation" (Clebsch et al., 2015)

### ADR-007: CRDTs Over Consensus for Distributed State

**Status:** Accepted
**Context:** Strongly consistent replicated state requires consensus (Paxos/Raft), which has high latency and is unavailable during partitions. Many distributed use cases (shopping carts, presence, counters) can tolerate eventual consistency.
**Decision:** Nulang uses CRDTs (Conflict-Free Replicated Data Types) as the default for geo-replicated state. Strong consistency is available but opt-in and requires consensus.
**Consequences:** (+) Low latency everywhere, available during partitions, no coordination overhead. (-) Eventual consistency (stale reads possible), limited CRDT types (not all data structures have CRDT equivalents), larger state (need to track tombstones).
**References:** "Conflict-Free Replicated Data Types" (Shapiro et al., 2011), "A Comprehensive Study of Convergent and Commutative Replicated Data Types"

### ADR-008: Algebraic Effects for Side Effects

**Status:** Accepted
**Context:** Side effects (IO, time, randomness) must be tracked for deterministic replay and capability-based security. Options: monadic IO (Haskell — too invasive), unchecked effects (most languages — unsafe), effect handlers (Koka, Eff — right abstraction).
**Decision:** Nulang uses row-polymorphic algebraic effects. Effects are typed, resumable, and tracked in function signatures. Effect results are captured for deterministic replay.
**Consequences:** (+) Clean separation of what from how, testable via handler swapping, automatic tracing, deterministic replay. (-) Effect row syntax adds noise to types, effect handlers are a new concept for many developers.
**References:** Koka (Leijen), Eff (Bauer & Pretnar), "Algebraic Effects for Functional Programming" (Plotkin & Power, 2002)

### ADR-009: Capabilities Map to WASI

**Status:** Superseded (2026-07) — capabilities are Pony-style reference capabilities enforced at compile time and erased at runtime (§2.4); no WASI mapping exists. The authority-token model (narrowing, revocation) remains target design.
**Context:** Nulang's capability security model needs an implementation mechanism. Inventing a custom capability system would require custom tooling and verification.
**Decision:** Nulang capabilities compile to WASI world imports. The WASM Component Model's import system IS the capability system. If an actor doesn't import `wasi:http/outgoing-handler`, it cannot make HTTP requests — enforced by the WASM sandbox.
**Consequences:** (+) Zero-overhead capability enforcement (sandbox-level, not runtime checks), standard tooling, composable via WIT. (-) Tied to WASI/WASM ecosystem evolution, some capabilities need custom WIT interfaces.
**References:** WASI (WebAssembly System Interface), WASM Component Model, "Capability-Based Security and the WASM Component Model"

### ADR-010: Gossip-Based Cluster Membership

**Status:** Accepted
**Context:** Cluster membership and failure detection need to be decentralized, scalable, and tolerant of network partitions. Options: centralized coordinator (single point of failure), consensus-based (too slow), gossip-based (eventual, scalable).
**Decision:** Nulang uses the SWIM gossip protocol for cluster membership and failure detection. Node state is disseminated via gossip. Actor placement changes propagate via gossip piggybacking.
**Consequences:** (+) Decentralized, scalable to 1000+ nodes, sub-second failure detection, minimal network overhead. (-) Eventual consistency of membership view, false positives under high packet loss, no global consistency guarantee.
**References:** SWIM (Scalable Weakly-consistent Infection-style Process Group Membership Protocol) (Das et al., 2002), HashiCorp Memberlist

### ADR-011: Indentation-Sensitive Syntax

**Status:** Superseded (2026-07) — the implemented syntax is brace-based and indentation-insensitive (§2.1); newlines are expression terminators, not block structure.
**Context:** Syntax style debates waste time. Braces (C-style) are redundant with proper indentation. Python proved indentation-based syntax works at scale.
**Decision:** Nulang uses mandatory 2-space indentation. No braces, no semicolons, no configuration. A deterministic formatter is required (like gofmt).
**Consequences:** (+) Visual hierarchy matches semantic hierarchy, no style debates, smaller code, enforced consistency. (-) Whitespace sensitivity can surprise newcomers, copy-paste can break indentation, hard to generate programmatically.
**References:** Python, Haskell, F#, gofmt

### ADR-012: No Async/Await Syntax

**Status:** Accepted
**Context:** Async/await creates "colored functions" — sync functions can't call async functions, leading to contagious `async` annotations throughout codebases. In an actor model, all concurrency is via message passing, not async tasks.
**Decision:** Nulang has no async/await syntax. Actors process messages sequentially within a behavior handler. Concurrency comes from having many actors. Effect handlers may be async internally, but this is invisible to actor code.
**Consequences:** (+) No colored functions, simpler mental model, behavior handlers are atomic from the actor's perspective. (-) Cannot express intra-actor concurrency, long operations must be split into multiple messages.
**References:** "What Color is Your Function?" (Nathaniel Smith, 2015), Erlang, original actor model (Hewitt et al., 1973)

---

## 10. Performance Targets

These targets define the performance envelope for a production Nulang deployment on commodity hardware (AMD EPYC / Intel Xeon, NVMe SSD, 10Gbps network).

### 10.1 Actor Lifecycle

| Metric | Target | Measurement | Notes |
|--------|--------|-------------|-------|
| Actor creation (activation) | < 1 microsecond | Time from first message to behavior handler start | Warm WASM instance from pool |
| Actor creation (cold start) | < 10 milliseconds | Time when WASM instance must be compiled | JIT compilation cost |
| Actor deactivation | < 1 millisecond | Time to checkpoint and release resources | Excluding storage write latency |
| Actor migration (same DC) | < 5 milliseconds | Time to stop on source, start on target | State transfer via shared storage |

### 10.2 Messaging

| Metric | Target | Measurement | Notes |
|--------|--------|-------------|-------|
| Message send (local) | < 100 nanoseconds | Time to enqueue in mailbox | Lock-free ring buffer |
| Message send (remote, same DC) | < 1 millisecond | End-to-end including serialization | TLS + TCP, under 1ms at p99 |
| Message send (cross-region) | < 50 milliseconds | Transatlantic / transpacific | Depends on geography |
| Message processing throughput | > 1M messages/second/node | Sustained, `local` actors | 64-byte messages, batching enabled |
| Mailbox capacity | 10,000 messages | Default, configurable | Bounded to prevent OOM |

### 10.3 Durability

| Metric | Target | Measurement | Notes |
|--------|--------|-------------|-------|
| Checkpoint latency | < 10 milliseconds | COW snapshot + serialization | Excluding storage backend write |
| Storage write (SQLite) | < 1 millisecond | Append to local SQLite | NVMe SSD |
| Storage write (PostgreSQL) | < 5 milliseconds | Network roundtrip to DB | Same datacenter |
| Storage write (S3) | < 50 milliseconds | HTTP PUT to object store | Regional endpoint |
| Event journal append | < 1 millisecond | Local segment file append | Sequential write, fsync optional |
| Replay throughput | > 100K events/second | Events replayed per second | From SSD, single-threaded |
| Recovery time | < 5 seconds | Node failure to all actors resumed | 100K actors, NVMe storage |

### 10.4 Scheduling

| Metric | Target | Measurement | Notes |
|--------|--------|-------------|-------|
| Scheduler latency | < 10 microseconds | Time from message arrival to actor scheduled | Work-stealing efficiency |
| Context switch (actor) | < 50 nanoseconds | WASM instance swap | Instance pool hot path |
| Fairness guarantee | Max 2x ideal share | No actor starves others | Reduction counting |
| Max actors per node | > 1,000,000 | Active + deactivated (memory-bound) | 1KB average per actor |
| Max active actors | > 100,000 | Actually scheduled | CPU-bound |

### 10.5 Garbage Collection

| Metric | Target | Measurement | Notes |
|--------|--------|-------------|-------|
| GC pause (per-actor) | < 1 millisecond | Stop time for collection | ORCA cycle detection |
| GC overhead | < 5% | CPU time spent in GC | Reference counting dominant |
| Memory per actor | < 1KB baseline | Empty actor overhead | WASM instance shared |
| Heap limit per actor | 100MB default, 1GB max | Configurable | Enforced by WASM memory bounds |

### 10.6 Distributed

| Metric | Target | Measurement | Notes |
|--------|--------|-------------|-------|
| Cluster size | > 100 nodes | Tested, supported | Gossip protocol scales sub-linearly |
| Failure detection | < 3 seconds | Time to detect node failure | SWIM with suspicion mechanism |
| Gossip propagation | < 500ms | Time for update to reach all nodes | 100-node cluster |
| CRDT sync latency | < 100ms eventual | Time to converge after update | Same region, periodic sync |
| CRDT sync (cross-region) | < 5 seconds | Time to converge globally | Periodic sync mode |
| Consistent hash lookup | < 1 microsecond | Identity to node resolution | In-memory ring |

### 10.7 AI Runtime

| Metric | Target | Measurement | Notes |
|--------|--------|-------------|-------|
| LLM call latency (local) | < 100ms | End-to-end, cached tools | Excluding model inference time |
| Tool registration | < 1ms | Schema generation + registry | Per tool, at deploy time |
| Tool execution | Same as message send | Tool is just an actor message | + LLM roundtrip |
| Vector query | < 10ms | Semantic similarity search | HNSW index, local Qdrant |
| Vector upsert | < 5ms | Add to vector store | Batched async |
| Trace emission | < 1ms | Fire-and-forget to collector | Async, buffered |

### 10.8 Workflow Throughput

| Metric | Target | Measurement | Notes |
|--------|--------|-------------|-------|
| Workflow start | < 5ms | Time from request to first step | Includes compilation for dynamic workflows |
| Step throughput | > 10K steps/second/node | Completed workflow steps | `durable` actors, local storage |
| Compensation execution | < 50ms per step | Time to execute compensation | Sequential, reverse order |
| Human-in-the-loop resume | < 2 seconds | Time from human action to workflow resume | Includes activation + replay |
| Parallel step fan-out | 10 steps default, 100 max | Concurrent parallel steps | Configurable |

---

## 11. References

### Academic Papers

1. Hewitt, C., Bishop, P., & Steiger, R. (1973). "A Universal Modular ACTOR Formalism for Artificial Intelligence." *IJCAI*.
2. Agha, G. (1986). *Actors: A Model of Concurrent Computation in Distributed Systems.* MIT Press.
3. Bernstein, P. A., et al. (2014). "Orleans: Distributed Virtual Actors for Programmability and Scalability." *Microsoft Research Technical Report*.
4. Shapiro, M., Preguica, N., Baquero, C., & Zawirski, M. (2011). "Conflict-Free Replicated Data Types." *SSS*.
5. Clebsch, S., Drossopoulou, S., Blessing, S., & McNeil, A. (2015). "Deny Capabilities for Safe, Fast Actors." *AGERE!*.
6. Das, A., Gupta, I., & Motivala, A. (2002). "SWIM: Scalable Weakly-consistent Infection-style Process Group Membership Protocol." *ICDCS*.
7. Leijen, D. (2017). "Type Directed Compilation of Row-Typed Algebraic Effects." *POPL*.
8. Plotkin, G., & Power, J. (2002). "Notions of Computation Determine Monads." *FOSSACS*.

### Industry Systems

1. **Erlang/OTP** — Ericsson. The original actor language and runtime.
2. **Orleans** — Microsoft. Virtual actors for .NET.
3. **Akka** — Lightbend. Actor toolkit for JVM (now Apache Pekko).
4. **Temporal** — Temporal Technologies. Durable execution platform.
5. **Dapr** — Microsoft. Distributed application runtime with building blocks.
6. **Wasmtime** — Bytecode Alliance. WASM runtime for the Component Model.
7. **Cloudflare Workers** — Edge WASM runtime (inspiration for deployment targets).

### Specifications

1. **WebAssembly Core Specification 2.0** — W3C.
2. **WebAssembly Component Model** — W3C Community Group.
3. **WASI Preview 2** — WebAssembly System Interface.
4. **OpenTelemetry** — CNCF observability standard.
5. **Cap'n Proto Serialization** — Cap'n Proto project.

---

*End of Architecture Reference*
