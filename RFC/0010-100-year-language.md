# RFC 0010: 100-Year Language Architecture

- **Status:** Draft
- **Tier:** N/A (design guidance, not surface change)
- **Author:** David Porkka (AI-assisted)
- **Created:** 2026-07-25
- **Resolved:** TBD
- **Language-version at effect:** N/A
- **Supersedes:** none
- **Superseded by:** RFC 0017 for unified-runtime/agent-workflow direction; RFC 0021 for pre-adoption source-freeze policy

## Summary

Documents architectural decisions and design principles that position Nulang
for relevance across a 100+ year horizon — spanning Von Neumann, non-Von
Neumann, quantum, and event-driven architectures, with small binaries,
powerful libraries, and no AI terminology lock-in. This RFC does not propose
surface changes; it records design rationale and recommends migration paths
for existing surfaces that couple the language to current paradigms.

## Motivation

Programming languages that survive decades do so by separating invariant
semantics from transient implementation details. C's longevity comes from its
thin abstraction over the machine; Erlang's from the actor model; Lisp's from
its minimal core. Nulang already has three strengths that map to longevity:
the actor model (concurrency primitive that predates multi-core and will
outlive it), algebraic effects (a generalization of exceptions, async, and
generators that composes cleanly), and a frozen Core subset (RFC 0002).
This RFC captures the remaining architectural decisions needed.

## Design

### C.1 Frozen Core as the Longevity Contract

The Core subset defined in RFC 0002 is the invariant kernel that every
conforming Nulang implementation must support. Its longevity properties:

- **Self-hosting path:** The bootstrap compiler (`bootstrap/compiler_core.nula`)
  is written in Core and targets the `.nbc` format (RFC 0001). Stage 1 compiles
  Core programs; Stage 2 compiles itself. This means Nulang can be ported to
  new hardware without the Rust toolchain — only a Core interpreter or compiler
  is needed, and the bootstrap compiler provides it.

- **`.nbc` format stability (RFC 0001):** Frozen bytecode artifacts carry a
  version number, BLAKE3 source hash, and language version. The format migration
  registry (`src/format/migrate.rs`) is the sole legal home for format upgrades.
  Old artifacts remain runnable as long as the migration path exists.

- **Value layout versioning:** The `VALUE_LAYOUT_VERSION` constant gates the
  i64-tagged representation. A future version can support 128-bit or
  variable-width tags without breaking the frozen Core semantics — only the
  runtime encoding changes.

- **Wire protocol versioning:** NUL0 v1 now begins with a 16-byte versioned
  handshake (`NUL0` magic + `version:u32` + `node_id:u64`) before framed
  packets. Published NUL0 v1 bytes are a Frozen compatibility obligation;
  higher-level actor/distribution source surfaces follow their own
  Stable/Experimental classifications.

**Recommendation:** These mechanisms already exist. No change required; this
section documents them as the longevity contract.

### C.2 Decouple AI Terminology from Language Surface

Current AI surface couples the language to the LLM paradigm:

| Current name | Problem | Recommendation |
|---|---|---|
| `LLM` effect (`LLM.ask`) | "Large Language Model" is a specific architecture | Rename to `Inference` effect (`Inference.ask`) — provider-agnostic, covers any ML inference |
| `@tool` annotation | Good as generic annotation; semantics are extensible | Keep as-is; document extensibility |
| `Pipeline` module | Hardcoded orchestration pattern | Move to standard library; language should not hardcode orchestration |
| `Supervisor` module | Same | Move to standard library |
| `Debate` module | Same | Move to standard library |
| `agent` keyword | Higher-level AI syntax can drift from the canonical runtime model | Per accepted RFC 0017, retain it only as Experimental ergonomic syntax that lowers to canonical actor/effect primitives |

**Migration path:**
1. `LLM` → `Inference`: Add `Inference` as an alias; deprecate `LLM` over one
   major version; remove in the following version.
2. `Pipeline`, `Supervisor`, `Debate`: Extract from built-in modules to a
   `nulang-ai` standard library package. The language surface should expose
   only `actor`, `perform Inference.ask(...)`, and `@tool`.
3. `agent` runtime unification: accepted RFC 0017 supersedes the removal
   recommendation. Keep `agent` Experimental and ensure its executable
   semantics lower to the canonical actor/effect model rather than a separate
   runtime species.

### C.3 Architecture-Independent Value Representation

Current i64-tagged value layout (48-bit payload, 16-bit tag) assumes 64-bit
architectures. For multi-architecture relevance:

- **Path to 128-bit:** The value layout version number allows a future layout
  with 112-bit payload and 16-bit tag, or a variable-width encoding. The Core
  semantics (integers are arbitrary-precision in the type system, bounded by
  `i64` at runtime) already distinguish logical from physical representation.

- **Endianness:** The `.nbc` format and NUL0 wire protocol use big-endian
  encoding. Future non-little-endian hardware (some quantum control processors,
  neuromorphic chips) would need explicit endianness negotiation. Add an
  endianness flag to the NUL0 handshake (`NUL0` magic + version + endianness
  byte) in a future protocol version.

- **Quantum computing path:** Values are classical; quantum operations would be
  a new effect (`Quantum`) with separate state. The effect system already
  isolates effects — a `Quantum` effect would carry its own qubit register
  state, invisible to classical code. No change to Core needed.

- **Tagged vs. uniform:** The Core type system (HM + capabilities + effect
  rows) is representation-agnostic. A future implementation could use uniform
  boxed values or a different tagging scheme without changing any Core program.

**Recommendation:** No surface change. Add endianness negotiation to the NUL0
protocol in a future protocol version. Document the value layout version as
the migration mechanism.

### C.4 Non-Von Neumann and Event-Driven Architecture Path

The actor model + algebraic effects maps naturally to diverse execution models:

- **Event-driven architectures:** Actors are event handlers. Messages are
  events. The mailbox is an event queue. The `receive` expression with
  selective matching (`ReceiveMatch` opcode) is event pattern-matching.
  No change needed — the actor model was designed for this.

- **Dataflow / non-Von Neumann:** Effect handlers can be compiled to dataflow
  graphs. A `perform` is a node activation; the continuation is a dataflow
  edge; `resume` is backpressure resolution. The MIR→bytecode pipeline already
  has a lowering phase (`src/mir_lower.rs`). A MIR→dataflow backend would be
  a new codegen target (`src/mir_dataflow.rs`), not a language change.

- **Spatial / reconfigurable (FPGA, CGRAs):** The MIR is a control-flow graph
  with basic blocks. A MIR→spatial backend would map basic blocks to
  reconfigurable logic regions, with `perform`/`handle` as region transitions.
  Again, a codegen target, not a language change.

- **Neuromorphic:** The actor model's message-passing maps to spiking neural
  networks: actors are neurons, messages are spikes, mailboxes are synaptic
  delays. The `Inference` effect (renamed from `LLM`, see C.2) would be the
  natural interface to neuromorphic accelerators.

**Recommendation:** Document these mappings. No surface change needed — the
existing architecture already decomposes cleanly across execution models.
The MIR layer is the right abstraction boundary for new backends.

### C.5 Small Binaries + Powerful Libraries

Current: single binary includes VM, JIT, WASM, AI, Python, LSP. For small
binaries and library distribution:

- **Feature flags already exist:** `--no-default-features` strips Python,
  SQLite, LSP, and AI runtime. Document minimal build sizes per feature set.
  A `bytecode-only` build (no JIT, no WASM, no native, no Python, no LSP, no
  AI) should fit in ~1 MB.

- **Pre-compiled library artifacts:** The `nula` package manager should support
  `.nbc` artifacts as library dependencies. A library author publishes
  type-checked, compiled `.nbc` files; consumers link them without source
  distribution. The `.nbc` format already carries type metadata (via the
  constant pool and function signatures) — extend it with an export table for
  library symbols.

- **Bootstrap compiler artifacts:** The bootstrap compiler produces `.nbc`
  artifacts that can be as small as hundreds of bytes for simple programs.
  This is the path to tiny standalone binaries: compile to `.nbc`, embed a
  minimal Core VM (~50 KB), ship as a single executable.

**Recommendation:** Document minimal build configurations. Extend the `nula`
package manager with `.nbc` library dependency support as a follow-up RFC.

### C.6 Syntax Stability

The keyword set in SPEC2.md §2.3 lists 57 keywords. Every reserved word is a
permanent tax on the namespace.

- **Freeze the current set** as the "Frozen Syntax" tier. Any new keyword must
  go through the RFC process and be gated by a language version.

- **Unwired reserved words:** The following keywords are reserved but not wired
  into the implementation: `priv`, `loop`, `node`, `monitor`, `link`, `exit`,
  `await`, `subworkflow`. These should either be wired (with RFCs) or removed
  from the lexer before 2.0. Reserved-but-unused keywords create confusion and
  block user identifiers for no benefit.

- **Deprecation path:** A keyword removed from Frozen tier must:
  1. Be marked deprecated in the lexer (warning on use as keyword, allowed as
     identifier with a migration warning)
  2. Remain reserved for one major version
  3. Be freed in the following major version
  4. Migration tooling (`nulang migrate`) rewrites affected source files

**Recommendation:** Before 2.0, audit the 57 keywords and either wire or
remove the 8 unwired ones. Document the keyword lifecycle in GOVERNANCE.md.

## Tier Classification

This RFC does not change any stability tier. The recommendations affect:

- **Frozen:** Value layout versioning, `.nbc` format, NUL0 protocol (already
  frozen). The endianness flag would be a backward-compatible extension.
- **Stable:** The `LLM` → `Inference` rename would be a Stable-tier change
  (deprecation cycle required).
- **Experimental:** `Pipeline`, `Supervisor`, `Debate` extraction to standard
  library is Experimental-tier (can change without deprecation).

## Backwards Compatibility

All recommendations are backward-compatible in their initial phase:

- `LLM` → `Inference`: Add alias, deprecate old name, remove later.
- `agent` → `actor`: Parser desugaring, no existing code breaks.
- `Pipeline`/`Supervisor`/`Debate` extraction: Keep existing imports working
  while adding standard library equivalents.
- Keyword removal: Deprecation cycle with migration tooling.

## References

- RFC 0001: Format Stability (`.nbc` format, migration registry)
- RFC 0002: Frozen Core (Core subset, bootstrap compiler)
- SPEC2.md §1.1a: Nulang Core
- SPEC2.md §2.3: Keywords
- `src/value_layout.rs`: Value layout constants and versioning
- `src/format/`: Format stability layer (`.nbc` artifacts, migration registry, NUL0 wire protocol)
- `src/runtime/network.rs`: NUL0 wire protocol (handshake, `Packet` enum)
- `src/stdlib.rs`: Built-in effect inventory (`IO`, `LLM`, `Timer`, `Signal`, `Actor`, `Otp`)
- `AGENTS.md`: Architecture overview, pipeline, value layout, distribution
