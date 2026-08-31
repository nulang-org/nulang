# Nulang Language Specification v2.0

## July 2026

---

# Forward

This document defines the Nulang programming language, version 2.0. It is intended as the authoritative reference for both language implementers and users, providing a complete and precise account of Nulang's syntax, semantics, type system, runtime model, and standard library.

Nulang 2.0 represents a significant architectural evolution from the 1.x series. Where the earlier specification treated AI agents, distributed computing, and persistence as separate subsystems accessed through domain-specific keywords (`agent`, `cluster`, `store`), version 2.0 unifies these concerns under a single, coherent abstraction: the actor. In Nulang 2.0, all concurrent and distributed computation is expressed through actors. AI capabilities are granted to actors through the capability system, not through a separate agent DSL. Durability is a property of actors, not a separate storage layer. Distribution is an emergent property of the actor runtime, not a bolt-on framework.

This unification yields a language with fewer primitives and greater compositional power. A programmer learns one abstraction—the actor with behaviors, state, and effects—and applies it uniformly from a single-threaded script to a globally distributed, durable workflow. AI agents are one composition of these primitives; they are not a separate language surface.

The specification is organized into five conceptual layers:

1. **The Language Layer** (Chapters 1–7) defines the core language: syntax, types, algebraic effects, capability-based security, expressions, and declarations. This layer is self-contained and can be implemented independently of any runtime.

2. **The Actor Runtime Layer** (Chapter 8) defines the actor model: how actors are declared, how they communicate via asynchronous message passing, how they manage state, and how they are supervised. This layer is the foundation upon which all higher layers are built.

3. **The Durable Execution Layer** (Chapter 9) extends the actor runtime with persistence. Persistent actors survive process restarts through automatic checkpointing, event journaling, deterministic replay, and snapshotting.

4. **The Distributed Platform Layer** (Chapter 12) extends the durable actor runtime across machine boundaries. Virtual actors are transparently activated on any cluster node (**Planned**). Messages are routed across the network. CRDT state converges automatically (**Planned** — the CRDT replication machinery is implemented and tested at the Rust level, but `state crdt` fields are not yet wired to it and behave as `durable`; see §9.10 and §12.5). Faults are contained and recovered.

5. **The AI Runtime Layer** (Chapter 11) provides language-integrated access to large language models, tool use, memory systems, and planning. AI capabilities are expressed through the same algebraic effect system used for IO and network effects, and are gated by the same capability-based security model.

Each chapter contains a detailed outline of all sections and subsections, followed by the prose and examples for those sections. Chapters 1 through 3 are fully written. Chapters 4 through 15 contain detailed outlines with section headings, descriptive bullet points, and at least one complete code example per chapter to illustrate the key concepts.

Unless otherwise noted, examples in sections describing *implemented* features are complete, syntactically valid Nulang programs or program fragments under the current compiler. Sections describing unimplemented features are marked **Planned**, and their examples are aspirational.

---

# Implementation Status (Current Alpha)

This document is the design target for Nulang 2.0. The implementation in this repository is an alpha (the v0.9 series) that realizes a substantial subset of the design. This section records, as of the current commit, what is implemented and what remains planned, so readers can distinguish descriptions of working behavior from aspirational ones. Sections that describe unimplemented surface are marked **Planned** inline.

> **Verification note (July 2026).** The syntax, keyword, and semantic claims in Chapters 1–12 and Appendices A–C were re-verified against the implementation in July 2026 — specifically `src/lexer.rs` (keyword inventory, literals, operators), `src/parser.rs` (grammar), `src/ast.rs` (AST shapes), `src/typechecker.rs` (inference, defaults), `src/effect_checker.rs` (effect rows, capability lattice, sendability), `src/vm.rs` (runtime effect dispatch, arithmetic), `src/hir_lower.rs` (pipe semantics, AI builtins), `src/main.rs` (CLI), and `src/fuzz.rs` (typechecker fuzzer). Chapters 13–15 and Appendix D describe planned surfaces and were only annotated as such, not verified line-by-line.

**Implemented and verified against the source tree:**

- Triple-quoted multi-line strings (`\"\"\"...\"\"\"`) and `\u{...}` unicode escapes: standard escapes processed inside triple-quoted strings; interpolation not supported inside them; surrogate/out-of-range code points rejected with a `LexError` — `src/lexer.rs`. (Stable)
- The core expression language: literals (`Int`, `Float`, `String`, `Bool`, `Unit`, `Nil`), `let` / `let rec` bindings with `in`, `fn` lambdas, tuples, records, arrays, `if`/`then`/`else`, `match` (wildcard, variable, literal, tuple, record, variant, and `@` alias patterns), blocks, the pipe operator `|>`, and the operator set of Chapter 2.
- Top-level declarations: `fn` (with `[T]` type parameters, `->` return types, `!` effect rows, `: cap` capability annotations, and `@tool` annotations), `type` (alias, record, and variant forms), `effect`, `actor` / `persistent actor`, `entity`, `organization`, `agent`, `workflow`, `module`, `import`, and `extern` FFI blocks.
- Hindley-Milner type inference (Algorithm W) over tuples, records, variants, arrays, function types carrying effect rows and capabilities, and `&cap T` reference types.
- Algebraic effects: `perform Effect.op(args)`, `handle body { | Effect.op(x) => value }`, closed and open effect rows written `{IO, FS}` and `{IO, | row}`, enforced `!` annotations on `fn` and `behavior` bodies, and runtime handlers with resume semantics.
- Reference capabilities `iso`, `trn`, `ref`, `val`, `box`, `tag`, plus `lineariso` with exactly-once consumption tracking. Capabilities are checked at compile time and erased at runtime. Sendability (`lineariso`, `iso`, `val`, `tag`) is enforced for message arguments.
- Actors and entities: `actor`, `persistent actor`, `organization` (desugars to `entity`), and `entity` (desugars to `persistent actor` with `event_sourced` as the default state model); `spawn Actor { field = value }`, `spawn Actor {} as "name"` for stable identity, `send actor behavior(args)` and `actor ! behavior(args)`, `ask actor behavior(args)`, `receive { | Behavior(x) => expr }`, `self.field` state access, and the four state models (`local`, `durable`, `event_sourced`, `crdt`).
- Persistence for `persistent actor`s: durable snapshot/journal recovery and event-sourced replay, backed by in-memory, JSON-file, and SQLite stores.
- Workflows: `workflow Name { step name { body } compensate { expr } ... }` with `parallel { ... }` step groups, saga compensation in reverse order, `perform Signal.wait("name")`, and `perform Timer.sleep("name", ms)`, all durable across restarts.
- The AI runtime: `agent` declarations with model, system prompt, tools, episodic/semantic/procedural memory, and pricing; the generic `PerformAsync` opcode dispatches LLM, Pipeline, Supervisor, and Debate effects via `effect_op` strings (e.g. `"Inference.ask"`, `"Pipeline.run"`); agent behaviors (`ask`, `usage`, `store_fact`, `recall`); tool schemas generated from `@tool` functions; and the `Pipeline`, `Supervisor`, and `Debate` orchestration builtins. The pure AI types live in the `nulang-ai` workspace crate (`crates/nulang-ai/`); the core crate re-exports them behind the `ai-runtime` feature flag.
- A register-based bytecode VM with a Cranelift JIT tiering path; an OTP-style supervision runtime (restart strategies and policies, links, monitors, exit signals); a distributed runtime (TCP wire protocol, gossip membership, location-transparent addressing — Experimental; the eight CRDT types exist and are tested only at the Rust embedder level, with no `.nula`-level surface — see §9.10); a REPL; and an LSP server.
- Typeclass declarations: `class`/`impl` with dictionary-passing transform for method calls on concrete types (Phase 4, Experimental). See `CHANGELOG.md`. **Verified 2026-08-02, constrained-generic crash fixed 2026-08-13:** literal-receiver dispatch works end-to-end (minimal declarations, two-concrete-type dispatch, missing-impl rejection, superclass syntax all confirmed against the real binary). The canonical constrained-generic case — a typeclass bound on a type-variable receiver (`fn eq_check[T: Eq](a: T, b: T) -> Bool { a.eq(b) }`) — used to type-check and then **crash at runtime** ("Not a function: nil"): the dictionary transform only resolved literal receivers, not type-variable ones. Fixed at the HIR level (`DictKind::Param`); the call site now passes the concrete dictionary argument (`infer_type_arg` → `_impl_Eq_Int`). Pinned by `conformance/behavior/typeclass_06_constrained_generic_runtime_crash.nula` (now passes, exit 0).
- Generics (`fn f[T](...)`, `type T[A] = ...`, §7.8): basic generics — one or more independent type parameters, per-callsite type inference, return-only type parameters — work end-to-end. **Verified 2026-08-02, both gaps fixed 2026-08-13:** (1) recursive generic ADTs can now be constructed — §7.8's `type Tree[T] = Leaf | Node((Tree[T], T, Tree[T]))` type-checks its own constructor call, pinned for two independent recursive shapes (`generics_03` accept, `generics_07` accept); (2) declared type parameters are now skolemized inside the function body — a generic function that pins its type parameter to a concrete type via an internal literal (`fn fresh[T]() -> T { 0 - 1 }`) is rejected AT THE DEFINITION (rigid placeholder cannot unify with `Int`), not at a later mismatched call site (`generics_08` expects the type error, exit 4). See `conformance/behavior/generics_03/07/08_*.nula`.
- Standard-library modules: `stdlib::core`, `stdlib::list`, `stdlib::string`, `stdlib::set`, `stdlib::map`, `stdlib::http` — resolved via `NULANG_STDLIB` or `src/stdlib/` (Experimental). See `CHANGELOG.md`.
- Error handling syntax: `catch expr => body`, `fail expr` (structured short-circuit return), `T ! E` return types, `?` operator (Stable). See `CHANGELOG.md`.
- Transport resilience: `send remote`, `ask remote` with timeout clauses, capability enforcement at call sites (Stable). See `CHANGELOG.md`.
- `after ms => expr` standalone sugar, desugared to `receive {} after ms => expr` (Stable). See `CHANGELOG.md` §RFC 0005/0007.
- `entity` keyword and event sourcing (RFC 0005/0007): `events`/`apply`/`emit` blocks, compile-time event validation (Stable).
- Migration contracts (RFC 0008): `version: N`, `migration from N to M { ... }` blocks inside entities. **Correction (verified 2026-08-02): this is NOT Stable -- it is functionally inert, not just incomplete.** The syntax parses and shallow-typechecks, but no trigger mechanism exists anywhere in the runtime: MIR lowering discards migration bodies entirely (only the bare version number survives into `ActorMeta`), `Runtime::recover_actor` has no version comparison or migration dispatch, and the persistence layer's snapshot/journal formats carry no version field at all. Empirically: a migration body that mutates state never applies, one that performs `IO.print` never runs, RFC-mandated validations (version-gap and downgrade rejection) are unenforced, and the RFC's own example syntax fails to compile (event-migration handler parameters are never bound in scope). See `conformance/behavior/migration_*.nula` for the full evidence trail.
- Organization primitives (RFC 0009): `organization` keyword desugars to `entity` with durable defaults (Stable in its narrowest reading, verified 2026-08-02: the keyword-level desugar is real and the full entity body grammar works inside it, but "durable defaults" means persistent-actor-with-event-sourced-defaults, byte-identical to `entity` -- there is no separate durable-by-default state model. RFC 0009's own additional surface -- governance blocks, member/child-spawn syntax, contract blocks -- is still Draft/Planned per the RFC itself and correctly rejected at parse time, not silently accepted). See `conformance/behavior/org_*.nula`.
- MIR register spilling: `SpillLoad`/`SpillStore` opcodes for functions exceeding 238 usable registers. **Caveat (verified 2026-08-02): the spilling mechanism itself is correct** (checked at 1, 127, and 160 spilled locals, each against an independently computed expected value, not just "doesn't crash") **but is unreachable for the largest functions the claim is meant to cover** -- the compiler frontend itself aborts with a real stack overflow (SIGABRT) on functions with roughly 286+ flat `let` statements (measured by binary search: 285 compiles, 286 aborts), a recursion-depth limit unrelated to the register allocator. See `conformance/behavior/spill_*.nula`.
- Formal semantics: capability lattice proofs in `spec/formal/capabilities.lean` (5 of 6 theorems proved — lattice laws + `cap_sendable`/`discharge_sendable`; `linear_at_most_once` is the one remaining `sorry`, documented as requiring the split-context refinement of `HasTypeCap`). Core HM type soundness in `spec/formal/types.lean` is **proved** (2026-08-14): `progress`/`preservation`/`type_soundness` are all machine-checked. See `spec/formal/README.md`.
- `::` import resolution: module paths via `import stdlib::set`, `import mypkg::utils::math` (Experimental).
- Content-addressed lockfile: `Nulang.lock` carries BLAKE3 `content_hash` per pinned package (Experimental, RFC 0003 Item 11).
- Self-hosting bootstrap compiler: Stage 10 — end-to-end hex → .nbc pipeline (`bootstrap/compiler_core.nula`, `compile_hex.nula`) supporting lexing, Pratt parsing, evaluation, arithmetic, let bindings, closures, `if`/`else`, comparisons, and booleans in Nulang Core. Remaining: HM type inference, MIR lowering, self-compilation.
- Backend trait boundary (RFC 0003 Item 6): `JitBackend`, `WasmBackend`, `CryptoProvider`, `ForeignInterop`, `HttpProvider` traits in `src/backends/mod.rs` (Stable).
- Structured error messages: `NuError` enum with error codes (`ErrorCode`), `expected`/`found` fields per variant, automatic fix suggestions via `error_code()` and `suggestion()`, and `format_rich()` for colorized multi-line diagnostics with source excerpts and carets — `src/types.rs`. (Stable)
- `**` exponentiation operator: tokenized as `Star2`, right-associative, precedence above `*` (Pratt `PREC_EXP` level 14, `src/parser.rs:45`), wired through parser, typechecker, HIR lowering, and bytecode — `src/lexer.rs`, `src/parser.rs`. (Stable)
- `catch` prefix syntax: `catch expr fallback` in addition to postfix `expr catch fallback` — `src/parser.rs`. (Stable)
- Spawn field-initializer overrides: `spawn A { f = v }` now correctly overrides the actor's declared default for field `f`; overrides are encoded in bytecode and applied at VM spawn time — `src/vm.rs`, `src/mir_codegen.rs`, `src/bytecode.rs`. (Stable)
- Clearer "cannot assign to immutable binding" diagnostic: the type error for reassigning a `let` binding now explains that mutable locals are not yet supported and suggests shadowing — `src/typechecker.rs`. (Stable)
- Let-chain stack-overflow fix: long chains of consecutive `let` bindings are flattened iteratively in both the parser (sequential `let`-statement peeling) and HIR lowering (`lower_let_chain`), eliminating deep-recursion overflow on blocks with 40+ lets — `src/parser.rs`, `src/hir_lower.rs`. (Stable)
- Package manager subcommands: `nula init` (scaffold a package), `nula list` (locked dependencies), `nula clean` (remove build artifacts), `nula add <name> [--path|--git|--version]`, `nula remove <name>`, `nula run --watch` / `nula watch` (re-run on source changes), and `nula doc [--open]` (generate Markdown API docs) — `src/package/commands.rs`. (Experimental)
- REPL enhancements: `:help <topic>` (topics: syntax, types, actors, effects, commands), `:load <file>` (load and evaluate a `.nula` file), `:type <expr>` (show inferred type without evaluating), tab completion for identifiers and REPL commands, automatic multi-line input when braces/parens/brackets are unclosed — `src/repl.rs`. (Experimental)
- New stdlib modules: `result` (Result combinators: `unwrap`, `map`, `flat_map`), `option` (Option combinators), `datetime` (DateTime record type), `math` (trigonometry, logarithms, rounding), `fs` (FS-effect wrapper), and `test` (Test-effect assertion helpers: `assert`, `assert_eq`, `assert_true`, `fail_with`) — `src/stdlib/`. (Experimental)
- `FS` filesystem effect: `perform FS.read(path)` → `String`, `perform FS.write(path, content)` → `Unit`, `perform FS.append(path, content)` → `Unit`, `perform FS.exists(path)` → `Bool`; built-in effect wired into the standalone VM — `src/stdlib/fs.nula`, `src/stdlib.rs`, `src/vm.rs`. (Experimental)
- `Test` assertion effect + `nula test [--filter <substr>]` runner: `perform Test.assert(cond, msg)`, `perform Test.assert_eq(a, b)`, `perform Test.assert_true(cond)`; the runner discovers `.nula` test files under `tests/`, reports pass/fail counts, supports name filtering — `src/stdlib/test.nula`, `src/package/commands.rs`. (Experimental)
- LSP enhancements: `.` and `::` completion trigger characters, field-access completion (on `self.` fields, record fields, actor state), `textDocument/didSave` handler that re-checks the file on save, completion items sorted by category (locals > functions > types > variants > keywords > effects) — `src/lsp/mod.rs`. (Experimental)
- `var` bindings: mutable local variables via `var x = 0` declaration and `x = expr` reassignment; tracked separately from `let` in typechecker and codegen — `src/parser.rs`, `src/typechecker.rs`, `src/mir_codegen.rs`. (Experimental)
- Record-update syntax: `{ base .. field = value }` creates a new record with overridden fields; parsed with `PREC_RANGE` precedence, disambiguated from range-in-block by checking for `=` — `src/parser.rs`. (Experimental)
- Tuple `.0`/`.1` field access: numeric indices on tuples (`t.0`, `t.1`); chained access (`t.0.1`) on nested tuples without parenthesization — `src/parser.rs`, `src/hir_lower.rs`. (Stable)
- Range expressions: `a .. b` inclusive-exclusive range at `PREC_RANGE` precedence (below pipe, above logical-or); works in `for` loops (`for i in 0 .. 5 { … }`) and bare in blocks (`{ a .. b }`) — `src/parser.rs`. (Experimental)
- `String.from_char`: `perform String.from_char(code)` creates a single-character string from a Unicode code point; returns `nil` for invalid code points — `src/stdlib.rs`, `src/vm.rs`. (Stable)
- `String.+` fix for let-bound variables: `a + b` where both operands are `let`-bound string variables now correctly concatenates — `src/vm.rs`. (Stable)
- `else`-on-newline fix: an `else` keyword following a newline after `}` is accepted in `if`/`else` chains — `src/parser.rs`. (Stable)
- `let..in` scoping fix: block-level `let x = V in BODY` correctly scopes `x` to `BODY` only — `src/hir_lower.rs`. (Stable)
- `Http` builtin effect: `perform Http.get(url)` and `perform Http.post(url, body)` wired into the standalone VM via `ureq`; returns response body as `String` — `src/stdlib.rs`, `src/vm.rs`. (Experimental)
- `Array` builtin effect: `perform Array.length(arr)`, `perform Array.push(arr, elem)`, `perform Array.new(n, init)`, `perform Array.set(arr, idx, val)`, `perform Array.slice(arr, start, end)` — value semantics, all return new arrays — `src/stdlib.rs`, `src/vm.rs`. (Experimental)
- Numeric conversion primitives: `Int.to_float`, `Float.to_int` (truncates toward zero), `Float.to_string`, `String.to_int` (returns 0 for invalid), `String.to_float` (returns 0.0 for invalid) — `src/stdlib.rs`, `src/vm.rs`. (Experimental)
- JSON parse + stringify: pure-Nulang recursive-descent parser in `stdlib::json` with `parse` and `stringify` functions; uses `String.to_float`, `Float.to_string`, `String.from_char`, and `Array.*` primitives — `src/stdlib/json.nula`. (Experimental)
- All 13 stdlib modules functional: `core`, `list`, `string`, `set`, `map`, `test`, `fs`, `option`, `result`, `datetime`, `math`, `json`, `http` — all parse, import, and resolve with all VM primitives available — `src/stdlib/`. (Experimental)
- LSP code lenses, document links, enriched hover: `textDocument/codeLens` shows reference counts; `textDocument/documentLink` creates clickable import links; `textDocument/hover` includes doc comments, effects, and type signatures — `src/lsp/mod.rs`. (Experimental)
- LSP completion documentation: keyword and built-in effect completion items carry markdown documentation strings — `src/lsp/mod.rs`. (Experimental)
- 15 verified example programs under `examples/` with `examples/README.md` — from basic IO to JSON, HTTP, Option/Result, and ranges. (Experimental)
- `consume` / `recover` expressions: `consume x` marks a linear (`lineariso`) variable as consumed (reusing the existing at-most-once tracker); `recover { body }` is an isolated scope whose result must be sendable (checked in `src/effect_checker.rs`; the typechecker infers the body's type unchanged and lowering is transparent — it does **not** wrap the result in `Ok`/`Error`). See §3.9.2. Commit `e0cf432`. (Experimental)

**Planned (described in this specification, not implemented):**

- The WebAssembly compilation target (Chapter 13): WASM compilation exists behind the `wasm-backend` feature flag via `--backend wasm|wasm-run|wasm-aot`. WIT interface generation and WASI worlds are not yet implemented.
- Higher-kinded types, `Char` and `Decimal` primitives, character literals (Sections 2.4, 3.6).
- `<-` message syntax and indentation-based layout (Section 2.8).
- Authority capabilities (`capability` declarations on actors, delegation, revocation, auditing — Sections 1.5 and 5.3–5.6), `config` blocks, the `tool` declaration form inside actors, `virtual` actors, `select`, `await`, `await_human`, `sleep_until`, and `retry` blocks.
- The deployment manifest (`nulang.toml`), `nulang migrate`, and `nulang shell` (Chapter 15, Appendix D).

Five keywords formerly reserved (`where`, `priv`, `loop`, `node`,
`subworkflow`) have been removed from the lexer per RFC 0010 and now lex
as plain identifiers. `await` has been **re-reserved** (July 2026) as a
keyword for future async/await support; it is a reserved word that cannot
be used as an identifier. `link`, `monitor`, and `exit` are wired into
`spawn link/monitor` and `Actor.exit` syntax. `case` is accepted as an
optional match-arm prefix.

Where a section is marked **Planned**, its examples show the intended v2.0 syntax and may not parse under the current compiler.

---
# Format Stability

This chapter is the **stability contract** for Nulang's durable formats. It is
ratified by RFC 0001 (see `RFC/0001-format-stability.md`) and is part of the
Frozen tier (see `CHANGELOG.md`): the contracts below may not be weakened
without a new RFC and a major-version bump.

## FS.1 The Three Frozen Formats

Nulang has three versioned, frozen binary formats. Each carries a magic-bytes
identifier and a version number in every emitted artifact and on every wire
connection. A runtime that encounters a version it does not understand MUST
reject it with a named error; it MUST NOT reinterpret the bytes under a
different layout.

| Format | Magic | Version field | Source of truth |
| `.nbc` bytecode artifact | `NLBC` (4 bytes) | `format_version: u32` | `src/format/constants.rs` |
| NUL0 wire protocol | `NUL0` (4 bytes) | `version: u32` in handshake | `src/format/constants.rs` |
| Value layout (i64-tagged) | — | `VALUE_LAYOUT_VERSION: u32` | `src/value_layout.rs` |

The canonical constants live in `src/format/constants.rs`; no other module
defines format magic bytes or version numbers. Bumping any of these constants
is a breaking change requiring an RFC.

## FS.2 `.nbc` Byte Layout (Version 1)

A `.nbc` file is the durable, distributable encoding of a compiled module. All
integers are big-endian.

```text
offset  size           field
0       4              magic = b"NLBC"
4       4              format_version (u32)          — must be ≤ BYTECODE_MAX_VERSION
8       4              language_version (u32)        — recorded; checked against LANGUAGE_VERSION
12      32             source_hash (BLAKE3; 0x00..00 if unknown)
44      4              instr_count (u32)
48      4*instr_count  instructions (Instruction::encode() → u32)
48+4n   4              meta_len (u32)
52+4n   meta_len       metadata = JSON serialization of the module
```

The header is hand-rolled binary so a runtime can check magic + version in O(1)
without a serde dependency. The instruction stream is 4 bytes per instruction
(opcode `u8` + three `u8` operands, big-endian), so the format is coupled to
the frozen opcode values: an unknown opcode is rejected with
`FormatError::UnknownOpcode`, never reinterpreted. The metadata body is JSON,
universally parseable by any conforming runtime in any host language.

Codec: `CodeModule::to_nbc(source_hash) → Vec<u8>` and
`CodeModule::from_nbc(bytes) → Result<NbcArtifact, FormatError>` in
`src/format/nbc.rs`.

## FS.3 NUL0 Wire Handshake (Version 1)

Immediately after a TCP connection is established, both sides exchange a
16-byte handshake before any framed packets:

```text
[0..4]   magic "NUL0"   (WIRE_MAGIC)
[4..8]   version u32    (WIRE_VERSION, big-endian)
[8..16]  node_id u64    (big-endian)
```

A peer whose magic does not match or whose version does not equal
`WIRE_VERSION` is refused with an `io::Error` of kind `InvalidData`. The
negotiated version governs the packet framing for the lifetime of the
connection; packets themselves carry the existing `[len u32][magic "NUL0"]
[type u8][seq u64][payload]` framing unchanged.

## FS.4 Stability Contract

1. **A version, once assigned, is frozen.** The byte layout for version *N*
   never changes after publication. A "change" to a published version is a bug
   fix to a *deviation* from the published layout, not a layout change.

2. **Opcode values are never reused.** New opcodes are appended at the next
   free discriminant; an opcode removed in a major version leaves its value
   permanently retired. An artifact containing a retired opcode is rejected
   with `FormatError::UnknownOpcode`.

3. **Additive extensions only within a major version.** A new optional section
   may be appended to the metadata body; older runtimes ignore unknown JSON
   fields (serde default-deserializes them). A new required field or a changed
   field meaning requires a new format version and a migration.

4. **Migrations are pure, deterministic, append-only.** A `vN → v(N+1)`
   migration is registered once in `src/format/migrate.rs` and never modified
   thereafter. `migrate_nbc(bytes, target)` walks the chain so a runtime
   speaking v(N+1) loads a vN artifact by running the registered steps.

5. **Language version is distinct from crate version.** The crate
   (`Cargo.toml`) version may rev freely; the *language* version
   (`LANGUAGE_VERSION`, recorded in every `.nbc`) moves only on
   RFC-ratified change. An artifact whose `language_version` exceeds the
   runtime's is rejected with `FormatError::IncompatibleLanguage`.

## FS.5 Reference: `src/format/`

- `constants.rs` — `BYTECODE_MAGIC`, `BYTECODE_VERSION`, `BYTECODE_MAX_VERSION`,
  `WIRE_MAGIC`, `WIRE_VERSION`, `VALUE_LAYOUT_VERSION`, `LANGUAGE_VERSION`,
  `FormatError`.
- `nbc.rs` — `CodeModule::to_nbc` / `from_nbc`, `NbcArtifact`.
- `migrate.rs` — `peek_format_version`, `migrate_nbc` (the only legal home for
  format-version upgrades).

---


# Table of Contents

- Chapter 1: Introduction
- Chapter 2: Lexical Structure
- Chapter 3: Types
- Chapter 4: Effects
- Chapter 5: Capabilities
- Chapter 6: Expressions
- Chapter 7: Declarations
- Chapter 8: Actors
- Chapter 9: Persistent Actors
- Chapter 10: Workflows
- Chapter 11: AI Runtime
- Chapter 12: Distributed Runtime
- Chapter 13: WebAssembly Integration
- Chapter 14: Standard Library
- Chapter 15: Operational Model
- Appendix A: Grammar Reference
- Appendix B: Built-in Types Reference
- Appendix C: Effect Reference
- Appendix D: Migration Guide from v1 to v2

---

# Chapter 1: Introduction

## 1.1 What is Nulang?

Nulang is a durable computation language for long-lived, distributed, stateful software entities. It compiles to WebAssembly and runs on a purpose-built actor runtime that provides persistence, clustering, and effect-composed capabilities as first-class language features. AI agents, workflows, databases, and autonomous organizations are important applications of these primitives, not part of the language kernel.

Nulang occupies a distinctive position in the language design space. Like Erlang and Elixir, it is built on the actor model of concurrency, where independent computational entities communicate exclusively through asynchronous message passing. Like Rust, it employs a sophisticated type system with affine reference capabilities to guarantee memory safety and data-race freedom at compile time. Like Koka and Eff, it uses algebraic effects as the primary mechanism for defining, composing, and handling computational effects such as IO, exceptions, state mutation, storage, messaging, and time. AI model inference is one application of this mechanism, implemented through Cloud SDK libraries rather than as a language primitive. Like modern workflow orchestration systems, it provides durable execution semantics where long-running processes survive crashes and restarts automatically.

The synthesis of these features produces a language with four defining characteristics:

**Concurrency without locks.** Nulang actors do not share mutable memory. All communication is asynchronous and message-based, eliminating data races by construction. The type system enforces that mutable references cannot escape an actor's boundary.

**Effects as a unifying abstraction.** All side effects in Nulang—reading a file, making an HTTP request, querying a database, invoking an AI model, or sending a message—are expressed through a single mechanism: algebraic effects. An effect is declared, performed, and handled within a well-typed framework that makes effectful dependencies explicit in function signatures.

**Capability-based security.** Every reference in Nulang carries a capability that governs how it may be read, written, and shared. These capabilities form a lattice of authority that propagates through the program automatically. Combined with effect declarations, they provide a comprehensive security model: a function's type signature reveals exactly what it can do and what data it can access.

**Durable execution by default.** Any actor can be declared `persistent`, which enables automatic checkpointing, event journaling, and deterministic replay. Persistent actors form the building blocks of workflows—long-running compositions that survive crashes, support compensation, and orchestrate human-in-the-loop interactions.

## 1.1a Nulang Core (Frozen — RFC 0002)

Nulang **Core** is the minimal, frozen subset of the language. It is defined
in RFC 0002 and is the invariant kernel every conforming implementation must
support — including the self-hosting bootstrap compiler (`bootstrap/`). Core
consists of:

- **Expressions:** `fn`, `let`, `if`/`else`, `match`, closures, `return`.
- **Types:** `Int`, `Bool`, `String`, `Unit`, `Nil`; `Vec<T>`, `Map<K,V>`;
  tuples; records; `enum`. HM type inference over this subset.
- **Effects:** `IO.print` and `IO.read` only (terminal I/O).
- **Capabilities:** `val` only (immutable, sendable).

Core **excludes** actors, effects beyond IO, capabilities beyond `val`,
distribution, persistence, FFI, AI, JIT, WASM, and Python interop.
Every Core program is a valid Nulang program; every Core program valid
today is valid in every future language version.

The self-hosting bootstrap compiler (`bootstrap/compiler_core.nula`) is
written in Core and targets the `.nbc` format (RFC 0001). Stage 1 compiles
Core programs; Stage 2 compiles itself. The Rust implementation remains the
fast path; the bootstrap compiler is the longevity path.

**Note:** Core gained `String.charAt` and `String.length` (2026-07-23)
primitives via `perform String.length(s)` and `perform String.charAt(s, i)`, which unblocked the bootstrap compiler's lexer from iterating
source characters. See
`bootstrap/README.md`.


## 1.2 Design Philosophy

Nulang's design is guided by five principles that influence every aspect of the language, from syntax to runtime architecture.

**Composition over configuration.** Nulang prefers composable language primitives over framework-specific configuration. Distributed systems are built by composing actors, not by configuring cluster topologies. AI integration is achieved by performing effects, not by wiring model providers. Persistence is a keyword, not a deployment concern. This principle ensures that the full power of the language—types, effects, pattern matching, higher-order functions—is available at every layer of the system.

**Explicit is better than implicit.** Every effect a function can perform is visible in its type signature through effect rows. Every reference's sharing properties are visible through capability annotations. Every state model is declared explicitly. This explicitness makes programs easier to reason about, test, and audit. It also enables powerful program analyses: the compiler can verify that a function performs no network effects, that an actor's state is serializable, or that a workflow step is deterministic.

**Failure as a first-class concern.** Nulang treats failure as a normal condition to be handled, not an exceptional circumstance to be ignored. Actors are supervised by parent actors that define restart strategies. Messages that cannot be delivered are reported through links and monitors. Workflow steps that fail trigger compensation handlers. This supervision-oriented design, inherited from Erlang's "let it crash" philosophy, enables the construction of resilient systems that recover automatically from hardware failures, network partitions, and software bugs.

**Uniformity across scales.** The same actor abstraction works for a single-process application and a globally distributed cluster. A `persistent` actor on one node uses the same syntax and semantics as a `persistent` actor replicated across a hundred nodes. An effect performed in a unit test can be handled by a pure mock, while the same effect in production is handled by a system call. This uniformity reduces the conceptual surface area programmers must learn and enables code reuse across deployment scenarios.

**Composable capabilities.** External capabilities—whether an LLM, a vector index, a billing meter, or a remote API—are accessed through the algebraic effect system and governed by the capability security model. A function that invokes an external capability declares the corresponding effect in its type signature. An actor that uses AI does so by importing a Cloud SDK library (for example, `nlc.ai`) and holding any required capability. This composability keeps the language kernel small while allowing powerful, statically traceable integrations to evolve as libraries.

## 1.3 The Actor as Universal Abstraction

The actor is the fundamental unit of computation, concurrency, state, and distribution in Nulang. Every running program consists of a tree of actors, each with its own mailbox, behaviors, and optionally persistent state.

An actor is declared with the `actor` keyword, followed by a name, optional type parameters, and a body containing state declarations and behaviors. Inside behaviors, state is read and written through `self`:

```nulang
actor Counter {
  state local count: Int = 0

  behavior increment(by: Int) {
    self.count = self.count + by
  }

  behavior get() {
    self.count
  }

  behavior reset() {
    self.count = 0
  }
}
```

Actors communicate exclusively through asynchronous messages. Sending a message is a non-blocking operation that places the message in the recipient's mailbox. The recipient processes messages sequentially, one at a time, guaranteeing that an actor's behavior handlers execute atomically with respect to each other. This single-threaded illusion within each actor eliminates the need for locks or other synchronization primitives.

Actors can be made persistent by adding the `persistent` keyword:

```nulang
type Result[T, E] = Ok(T) | Error(E)

persistent actor BankAccount {
  state durable balance: Int = 0

  behavior deposit(amount: Int) {
    self.balance = self.balance + amount
  }

  behavior withdraw(amount: Int) {
    if amount > self.balance then
      Error("Insufficient funds")
    else {
      self.balance = self.balance - amount
      Ok(unit)
    }
  }

  behavior get_balance() {
    self.balance
  }
}
```

(`Int` is used here because exact-decimal arithmetic — the `Decimal` type — is planned rather than implemented; see §3.2.4.)

The `persistent` keyword enables automatic checkpointing after each behavior invocation, ensuring that the actor's state survives process restarts. The `durable` state model (one of four available) guarantees that `balance` is written to persistent storage before the behavior returns.

Actors perform effects through the effect system, with their effect rows making authority explicit in the type signature. Inference (formerly "LLM") is itself an effect — `perform Inference.ask(...)` — wired to the agent runtime (Chapter 11). The legacy name `LLM` remains accepted as a deprecated alias:

```nulang
actor ChatBot {
  state local turns: Int = 0

  behavior ask(question: String) ! {Inference} {
    let answer = perform Inference.ask(question) in {
      self.turns = self.turns + 1
      answer
    }
  }
}
```

The `! {Inference}` row declares that this behavior may perform inference effects; the deprecated `{LLM}` row is also accepted. Performing an effect outside the declared row is a compile-time error. Authority capabilities (`capability llm`) that grant and revoke such authority per actor are planned — see §5.3.

## 1.4 State Models Overview

Every state variable in an actor has an associated *state model* that determines how it is stored, replicated, and recovered. Nulang provides four state models:

| Model | Persistence | Replication | Recovery | Use Case |
|-------|------------|-------------|----------|----------|
| `local` | None | None | Reset to initial value | Ephemeral caches, temporary buffers |
| `durable` | Snapshot + journal | None | Replay from journal | Single-node persistent state |
| `event_sourced` | Event journal | Event stream | Full event replay | Audit trails, temporal queries |
| `crdt` | Delta log | Concrete-type selector + merge-on-sync landed; `Crdt.*` effect module and operation-set enforcement open (see §9.10) | CRDT merge (on sync) | Shared distributed state |

The state model is declared alongside the variable:

```nulang
persistent actor ShoppingCart {
  state durable items_count: Int = 0
  state crdt    viewers: Int = 0
  state local   temp_discount: Int = 0

  behavior add_item(item_id: Int) {
    self.items_count = self.items_count + 1
  }

  behavior apply_discount(code: Int) {
    // Temporary, not persisted
    self.temp_discount = code
  }

  behavior track_viewer(node: Int) {
    // NOTE: `crdt` fields with a concrete-type selector route through
    // `CrdtManager` (merge-on-sync); see §9.10.
    self.viewers = self.viewers + 1
  }
}
```

The runtime enforces the semantics of `durable` (checkpointed and
journaled) and `event_sourced` (rebuilt by replaying emitted events, see
`emit`, §6.14). `crdt` fields with a concrete-type selector route through
`CrdtManager` (merge-on-sync); a `Crdt.*` effect module and operation-set
enforcement are not yet implemented — see §9.10/§12.5.

## 1.5 Capability Security Overview

Nulang employs two complementary capability systems: reference capabilities (which control how data can be aliased and shared) and authority capabilities (which control what effects an actor can perform).

Reference capabilities are part of the type system. Every reference has one of seven capabilities:

- `lineariso` — unique and linear, sendable, must be consumed exactly once
- `iso` — unique, sendable (no other references exist)
- `trn` — unique but locally writable (transitioning to `val`)
- `ref` — uniquely writable but not sendable
- `val` — immutable and sendable
- `box` — read-only view of `ref` or `val`
- `tag` — opaque identifier, not readable

These capabilities form a lattice under a subtyping relation (§3.9.1). The compiler uses them to guarantee that no data race can occur: a `lineariso`, `iso`, `val`, or `tag` reference can be sent to another actor because the sender cannot retain the ability to mutate the data. Reference capabilities are checked at compile time and erased before execution.

Authority capabilities — declared on actors to govern which effects they can perform — are planned (§5.3). The intended surface is:

```nulang
// fragment
// Planned — not yet implemented
capability llm      // Can perform LLM effects
capability http     // Can make HTTP requests
capability file     // Can access the file system
capability network  // Can open network connections
capability random   // Can access random number generation
capability time     // Can access the system clock
```

Authority capabilities are designed to be delegated from one actor to another and revoked at any time. This enables fine-grained security policies: an AI agent can be given `llm` and `http` capabilities, but not `file` or `network`.

## 1.6 Relationship to Other Languages

Nulang's design synthesizes ideas from several language families:

**From the actor languages (Erlang, Elixir, Pony, Akka):** The actor model as the fundamental concurrency primitive, supervision trees for fault tolerance, and the philosophy of isolated mutable state. Nulang differs in its static type system, algebraic effects, and unified treatment of persistence and distribution.

**From the effect languages (Koka, Eff, Flix):** Algebraic effects and handlers as the primary abstraction for computational effects, and effect rows in function types. Nulang extends this to include LLM calls as just another effect, and integrates effects with the actor model so that effect handlers can be actor-local.

**From the capability languages (Pony, Rust, Wyvern):** Reference capabilities for memory safety and data-race freedom. Nulang's capability system is most closely related to Pony's, but extends it with authority capabilities and distributed capability delegation.

**From the workflow languages (Temporal, Durable Functions):** Durable execution, deterministic replay, and saga compensation. Nulang embeds these concepts into the actor model rather than providing them as a separate framework.

**From the ML family (OCaml, Haskell, F#, Elm):** Hindley-Milner type inference, algebraic data types, pattern matching, and higher-order functions. Nulang's type system is closest to Elm's in its simplicity and inferability, extended with reference capabilities and effect rows.

---

# Chapter 2: Lexical Structure

## 2.1 Source Files and Encoding

Nulang source files conventionally use the `.nula` extension; the CLI accepts any path. A source file is a sequence of Unicode code points encoded in UTF-8. A leading UTF-8 Byte Order Mark is **not** currently stripped; source files should be saved without one.

A source file consists of a sequence of declarations: functions, type definitions, actor and agent definitions, effect definitions, workflow definitions, imports, and module-level expressions. There is no statement terminator; declarations and expressions are separated by newlines or semicolons. Blocks are delimited by braces, and newlines are tokens the parser uses to find expression boundaries — indentation itself is not significant (see Section 2.8).

A minimal Nulang program is a single module file that need not contain a `main` function. Each module-level expression is wrapped in a synthetic `__main` function and evaluated in order when the program starts, and any spawned actors continue running:

```nulang
// hello.nula: a minimal Nulang program
perform IO.print("Hello, World!")
```

If the module declares `fn main()`, that function is the entry point instead. Programs compile to bytecode for the Nulang register VM, which initializes the runtime, evaluates the entry function, and starts the actor scheduler. (Compilation to WebAssembly with a `__nulang_start` export is **Planned**; see Chapter 13.)

## 2.2 Comments

Nulang supports two comment styles:

**Line comments** begin with `//` and extend to the end of the line:

```nulang
// This is a line comment
let x = 42 in x  // Comments can also follow code on the same line
```

**Block comments** are delimited by `/*` and `*/`. Block comments may be nested, which allows commenting out code that itself contains block comments:

```nulang
/* This is a block comment.
   It can span multiple lines.
   /* And they can be nested. */
*/
```

Comments are treated as whitespace by the parser and have no semantic significance. They may appear between any two tokens. (An unterminated block comment currently consumes input to end-of-file rather than producing a lex error.)

**Documentation comments** are line comments beginning with `///`. They are preserved by the lexer as doc-comment tokens (ordinary comments are discarded) so tooling can associate them with the declaration that follows:

```nulang
/// Calculate the factorial of a non-negative integer.
/// Returns 1 for n = 0, and n * factorial(n - 1) otherwise.
fn factorial(n: Int) -> Int {
  if n == 0 then 1 else n * factorial(n - 1)
}
```

## 2.3 Keywords

The following identifiers are reserved as keywords in Nulang and may not be used as ordinary identifiers:

```
actor         durable       import        par           tag
agent         effect        in            parallel      then
alias         else          initial       perform       throws
and           emit          iso           persistent    tool
as            entity        let           pub           trn
ask           errdefer      linear        rec           true
await         event_sourced lineariso     receive       type
behavior      exit          link          recover       unit
box           extern        local         ref           until
break         fail          match         remote        using
case          false         migrate       resume        val
catch         fn            module        return        var
class         for           monitor       self          while
compensate    given         nil           send          with
consume       handle        not           spawn         workflow
crdt          handler       opaque        state
database      if            or            state_machine
defer         impl          organization  step
```

Keywords are case-sensitive and must be written in lowercase.

Notes on the inventory:

- `true`, `false`, `nil`, and `unit` are literal keywords, and `and`, `or`, `not` are keyword spellings of the `&&`, `||`, `!` operators.
- `entity` is a reserved keyword accepted by the grammar; it desugars to `persistent actor` with `event_sourced` as the default state model (see Chapter 8).
- `exit`, `link`, and `monitor` are used as operation names in `perform Actor.exit(...)` / `Actor.link(...)` / `Actor.monitor(...)`, and in `spawn link|monitor Actor { ... }` desugaring. `await` is reserved but unwired (re-reserved July 2026 for future async/await; see GOVERNANCE §2a). `where`, `priv`, `loop`, `node`, `subworkflow` were freed as identifiers per RFC 0010 §C.6.
- The capability words `iso`, `trn`, `ref`, `val`, `box`, `tag`, `lineariso`, `linear` are keywords usable anywhere a capability is parsed.
- `organization` is a reserved keyword accepted by the grammar; it desugars to `entity` with the same durable-first defaults (RFC 0009).
- `cap` (in the `expr :cap iso` annotation) and `to` (in `migrate a to node`) are contextual identifiers, not keywords.
- `var` (mutable binding), `consume`, `recover`, and `as` are keywords in the current lexer (`src/lexer.rs`). There is no `capability`, `enum`, `event`, `from`, or `config` keyword. Constructs earlier drafts associated with those words are either expressed differently (Chapters 5 and 7) or **Planned**.

## 2.4 Identifiers

An identifier begins with an ASCII letter (`a`–`z`, `A`–`Z`) or an underscore (`_`), followed by any number of ASCII letters, digits, or underscores. The current lexer is ASCII-only: non-ASCII letters (for example `α`) are rejected with a lex error. (Unicode identifiers are **Planned**.)

Identifiers beginning with an uppercase letter are lexed as *upper identifiers* and are used for type, variant-constructor, actor, and effect names. Both forms are otherwise ordinary identifiers.

Nulang uses the following naming conventions. They are conventions only — no style checker enforces them:

- **Types, variants, actors, and modules**: PascalCase (`String`, `Option`, `BankAccount`)
- **Functions and variables**: snake_case (`map`, `get_balance`, `process_request`)
- **Type variables in generics**: PascalCase, typically a single letter (`T`, `U`, `Elem`, `Key`)
- **Effect names**: PascalCase, short for the built-ins (`IO`, `Http`, `FS`, `Random`, `Time`, `Inference`; `Net` and `Rand` exist as compile-time aliases, §4.6/Appendix C)
- **Constants**: UPPER_SNAKE_CASE (`MAX_RETRIES`, `PI`)

Examples of valid identifiers:

```nulang
// fragment
name         _private     http2        x_y_z
Counter      Option       T            Elem
```

## 2.5 Literals

Nulang provides literals for the following types:

### 2.5.1 Integer Literals

Integer literals are sequences of decimal digits, or hexadecimal digits after a `0x` (or `0X`) prefix:

```nulang
42        // Decimal
0x2A      // Hexadecimal (= 42)
```

An integer literal has type `Int` (a 64-bit signed integer; see Section 3.2.2). A negative literal is written with unary negation (`-42`), which folds at compile time like any other unary expression. Octal (`0o52`), binary (`0b101010`), and underscore digit separators (`1_000_000`) are **Planned** — they are rejected by the current lexer.

### 2.5.2 Floating-Point Literals

Floating-point literals consist of an integer part, a decimal point, a fractional part, and optionally an exponent introduced by `e` or `E` with an optional sign:

```nulang
3.14159
2.99792458e8    // Scientific notation
1.0e-9          // Small numbers
```

A floating-point literal has type `Float` (IEEE 754 double precision). There are no `f32`/`f64` suffixes. A bare `1.` is not a float literal: a `.` is only consumed as a decimal point when followed by a digit, so `1..10` lexes as `1`, `..`, `10` (the range operator, §2.7).

### 2.5.3 String Literals

String literals are delimited by double quotes (`"`). They may contain any character except an unescaped double quote or backslash. The following escape sequences are recognized:

```nulang
"Hello, World!"
"Line 1\nLine 2"     // Newline
"Tab\tseparated"     // Tab
"Quote: \"hello\""   // Escaped quotes
"Backslash: \\"      // Escaped backslash
```

`\r` (carriage return) and `\0` (NUL) are also recognized. `\u{...}` Unicode escapes, triple-quoted multi-line strings, and `{expr}` string interpolation are **Planned**; none are accepted by the current lexer. (Template strings such as `"Research: {input}"` used with `Pipeline.stage` are plain string literals interpreted at runtime by the pipeline builtin — they are not language-level interpolation.)

### 2.5.4 Character Literals

**Planned.** There is no `Char` type and the lexer does not recognize single-quoted character literals.

### 2.5.5 Boolean Literals

The boolean literals are `true` and `false`, with type `Bool`.

### 2.5.6 Unit Literal

The unit literal is written `()` or with the keyword `unit`, both with type `Unit`. It represents the absence of a meaningful value and is the return type of operations that produce no data.

### 2.5.7 Nil Literal

The `nil` literal, with type `Nil`, represents the absence of a value (for example, a `receive` on an empty mailbox evaluates to `nil`).

## 2.6 Operators

The expression grammar is a Pratt parser with thirteen precedence levels. From loosest to tightest binding:

| Level | Operators | Associativity |
|-------|-----------|---------------|
| 1 | `=` `+=` `-=` | right |
| 2 | `\|>` | left |
| 3 | `\|\|` `or` | left |
| 4 | `&&` `and` | left |
| 5 | `==` `!=` | left |
| 6 | `<` `<=` `>` `>=` | left |
| 7 | `+` `-` | left |
| 8 | `*` `/` `%` | left |
| 9 | `<<` `>>` | left |
| 10 | `&` | left |
| 11 | `^` | left |
| 12 | `\|\|\|` | left |
| prefix | `!` `not` `-` `&` `*` | — |

Prefix operators bind at level 10. Postfix forms — function application `f(x)`, field access `x.f` / `x.0`, indexing `a[i]`, message send `a ! b(args)`, and annotations `e : T` / `e :cap c` — bind tighter than all binary operators.

Two quirks of the current grammar are worth noting:

- **Bitwise operators bind tighter than arithmetic.** `1 + 2 & 3` parses as `1 + (2 & 3)`. Use parentheses when mixing arithmetic and bitwise operators. (This ordering is inherited from the precedence table and may be revised before 2.0.)
- **Single `|` is not an infix operator.** It is reserved as the match-arm and variant separator, so bitwise OR is written `|||` (or the keyword `or` for booleans).

There is a `**` exponentiation operator (right-associative, binds tighter than `*`; `src/lexer.rs` `Star2`, `src/parser.rs` `PREC_EXP`). There is no `~` bitwise-not operator — the `~` token is lexed (`src/lexer.rs:1127`) but not accepted by the parser.

### 2.6.1 Arithmetic Operators

| Operator | Description | Example |
|----------|-------------|---------|
| `+` | Addition | `a + b` |
| `-` | Subtraction | `a - b` |
| `*` | Multiplication | `a * b` |
| `/` | Division | `a / b` |
| `%` | Remainder | `a % b` |
| `**` | Exponentiation (right-associative, binds tighter than `*`) | `a ** b` |
| `-` | Unary negation | `-x` |

Arithmetic operators are type-polymorphic through inference: both operands and the result share one type variable, so `+` works on `Int` or `Float` but the two operands must have the same type. Mixed-type arithmetic requires explicit conversion (conversion functions are **Planned** with the standard library). Division by zero evaluates to `nil`.

### 2.6.2 Comparison Operators

| Operator | Description |
|----------|-------------|
| `==` | Structural equality |
| `!=` | Structural inequality |
| `<` | Less than |
| `<=` | Less than or equal |
| `>` | Greater than |
| `>=` | Greater than or equal |

Comparison operators are left-associative like all binary operators, so `a < b < c` parses as `(a < b) < c`, which fails to type-check (`Bool` is not comparable to `c`). Write `(a < b) && (b < c)` instead.

### 2.6.3 Boolean Operators

| Operator | Description |
|----------|-------------|
| `&&` / `and` | Logical AND (short-circuiting) |
| `\|\|` / `or` | Logical OR (short-circuiting) |
| `!` / `not` | Logical NOT (unary prefix) |

The `&&` and `||` operators use short-circuit evaluation: the right operand is only evaluated if necessary.

### 2.6.4 Reference Capability Operators

| Operator | Description | Status |
|----------|-------------|--------|
| `&` | Create a reference (`&x`, capability `ref`) | Implemented |
| `*` | Dereference a reference (`*r`) | Implemented |
| `consume` | Consume a linear (`lineariso`) variable, marking it consumed in the at-most-once tracker | Implemented (Experimental) |
| `recover` | `recover { body }` — isolated scope whose result must be sendable (see §3.9.2) | Implemented (Experimental) |

Reference types and capabilities are discussed in detail in Chapter 5.

### 2.6.5 The Pipe Operator

The pipe operator `|>` passes the left operand as the **first** argument to the function on the right:

```nulang
// fragment
list |> map(f) |> filter(g)
// Equivalent to: filter(map(list, f), g)
```

The pipe operator has very low precedence (level 2, above only assignment) and is left-associative. It is described fully in Section 6.9.

## 2.7 Delimiters

Nulang uses the following delimiters:

| Delimiter | Usage |
|-----------|-------|
| `()` | Parentheses for grouping, tuples, and function arguments |
| `{}` | Braces for blocks, record literals, actor bodies, and effect rows |
| `[]` | Square brackets for array literals, indexing, and type parameters |
| `,` | Comma for separating elements |
| `:` | Colon for type annotations (`x : Int`) and capability annotations (`x :cap iso`) |
| `;` | Semicolon for separating expressions on the same line |
| `->` | Arrow for function types and `fn` bodies |
| `=>` | Fat arrow for match arms and handler clauses |
| `=` | Equals for bindings and assignment |
| `.` | Dot for field access (`r.name`) and tuple indexing (`t.0`) |
| `!` | Bang for message send (`a ! b(args)`) and effect annotations (`! {IO}`) |
| `\|` | Vertical bar introducing match arms, handler clauses, and variant constructors |
| `@` | At sign for annotations (`@tool(...)`) and pattern aliases (`n @ Some(x)`) |

The lexer also recognizes `..`, `::`, `<-`, and `?`, and the parser accepts all four today: `..` is the inclusive-exclusive range operator (`a..b`, Pratt `PREC_RANGE`, `src/parser.rs:83`) and the record-update separator (`{ base .. field = value }`, `src/parser.rs` `Expr::RecordUpdate`); `::` is the module-path separator in `import` declarations (`src/parser.rs` `parse_import`); `<-` is async tell, equivalent to `!` (`actor <- behavior(args)`, `src/parser.rs:2483`); and `?` is error propagation / try (`expr?` desugars to a `match` on `Ok`/`Error` with early `return`, `src/parser.rs:2603`) plus nil-safe optional chaining (`expr?.field`, `src/parser.rs:2567`).

## 2.8 Newlines, Semicolons, and Blocks

Block structure in the current grammar is explicit, not indentation-based:

- A **block** is a brace-delimited sequence of expressions, `{ e1; e2; …; en }`, whose value is the value of the last expression.
- **Newlines** are tokens. The parser skips newlines wherever an expression or declaration may continue, so expressions may span lines freely, and a newline (or a run of them) terminates an expression or declaration where one is complete.
- **Semicolons** separate expressions on the same line, exactly like newlines.

```nulang
let max = fn(a: Int, b: Int) {
  if a > b then a else b
} in
let m = max(3, 7) in
{ m; m + 1 }   // semicolons separate expressions; block value is m + 1
```

Indentation has no semantic significance; tabs and spaces are ordinary whitespace. (Indentation-sensitive layout, in the style of Haskell's offside rule, is **Planned** for a future revision.)

```nulang
// Example: an actor definition. Braces delimit the body; state fields
// and behaviors are separated by newlines.
actor WeatherService {
  state cache = 0

  behavior get_forecast(city: String) {
    match self.cache with {
      | 0 => self.cache
      | n => n
    }
  }
}
```

---

# Chapter 3: Types

## 3.1 Type System Overview

Nulang employs a static type system based on Hindley-Milner type inference with extensions for reference capabilities, effect rows, and generic programming. The type system has the following properties:

**Soundness.** Well-typed programs do not go wrong at runtime due to type errors. The type system prevents null pointer dereferences (through user-declared `Option`-style variants and the explicit `nil` value) and data races (through reference capabilities and the sendability check on messages).

**Complete inference.** The types of all expressions can be inferred automatically by the compiler (Hindley-Milner Algorithm W). Parameter type annotations are optional: unannotated parameters receive fresh type variables that unify with their uses. Annotations may be provided for documentation or to constrain inference, and they are required in `extern` FFI declarations.

**Parametric polymorphism.** Functions and types may be parameterized by type variables (`fn map[A, B](...)`, `type Pair[A, B] = ...`), enabling generic programming without runtime type checks. Let-bound values are generalized (let-polymorphism).

**Effect tracking.** Function types include an effect row that describes which computational effects the function may perform. This makes effectful dependencies explicit and enables local reasoning about code. Effect rows are inferred; a `! {Row}` annotation on a `fn` or `behavior` is enforced against the body's inferred row.

**Capability safety.** Reference types are qualified with capabilities that control how data can be read, written, and shared across actor boundaries. The capability system guarantees memory safety and data-race freedom and is checked entirely at compile time; capability annotations are erased before execution.

(Kinds and higher-kinded types are **Planned**; the current type system has a single kind `*` for ordinary types, and type constructors like `List` are not yet kind-checked.)

## 3.2 Primitive Types

Nulang provides the following primitive types:

### 3.2.1 Bool

The type `Bool` has two values: `true` and `false`. It supports the logical operators `&&`, `||`, and `!` (also spelled `and`, `or`, `not`).

```nulang
fn is_valid(x: Int) -> Bool {
  x > 0 && x < 100
}
```

### 3.2.2 Int

The type `Int` is a 64-bit signed integer (`i64`; range −9,223,372,036,854,775,808 to 9,223,372,036,854,775,807). It supports all arithmetic operators and the bitwise operations `&`, `^`, `|||`, `<<`, `>>`. (Single `|` is reserved for match arms — see Section 2.6.)

```nulang
fn double(x: Int) -> Int { x * 2 }
fn is_even(x: Int) -> Bool { x % 2 == 0 }
```

### 3.2.3 Float

The type `Float` represents IEEE 754 double-precision floating-point numbers (`f64`). (A single-precision `Float32` is **Planned**.)

```nulang
fn area(radius: Float) -> Float {
  3.14159 * radius * radius
}
```

### 3.2.4 Decimal

**Planned.** An arbitrary-precision `Decimal` type for financial calculations is not implemented. Use `Int` (scaled, e.g. cents) or `Float` today.

### 3.2.5 Char

**Planned.** There is no `Char` type; the lexer rejects single-quoted character literals.

### 3.2.6 Unit

The type `Unit` has a single value `()` (also written `unit`) and is used for functions and effects that return no meaningful value. It is analogous to `void` in C or `()` in Haskell.

### 3.2.7 Nil, Never, and Address

Three further primitive types complete the current set:

- `Nil` — the type of the `nil` literal (Section 2.5.7).
- `Never` — the empty type, used for computations that cannot produce a value.
- `Address` — the type of actor references (Section 8.10).

## 3.3 Product Types

Product types combine multiple values into a single value. Nulang provides two product type constructors: tuples and records.

### 3.3.1 Tuples

A tuple is an ordered collection of values of possibly different types. Tuple types are written with parentheses and commas; tuple values use the same syntax:

```nulang
let point: (Float, Float) = (3.0, 4.0) in point
let person: (String, Int, Bool) = ("Alice", 30, true) in person
```

Tuples are destructured with tuple patterns:

```nulang
let distance = fn(p: (Float, Float)) {
  match p with {
    | (x, y) => x * x + y * y
  }
} in distance((3.0, 4.0))
```

The empty tuple `()` is the same as the unit value. Single-element tuples `(a,)` are distinguished from parenthesized expressions by the trailing comma.

Tuple components are accessed by position using zero-based indexing: `point.0`, `point.1`.

### 3.3.2 Records

A record is a labeled product type, where each field has a name and a type. Record types are structural: two record types unify when they have the same field names with unifiable types, regardless of declaration order. Record literals and record types use a colon between the field name and its value or type:

```nulang
let person = { name: "Alice", age: 30, active: true } in
let greet = fn(p: { name: String, age: Int, active: Bool }) {
  p.name
} in
greet(person)
```

Record fields are accessed using dot notation: `person.name`, `person.age`. Record patterns destructure records with the same colon syntax: `{ name: n, age: a }`.

Structural update is implemented: `{ person .. age = 31 }` evaluates `person`, then produces a new record with the listed fields overridden (overrides use `=`; multiple overrides are comma-separated). Range expressions (`a..b`) are likewise implemented (`src/parser.rs` `Expr::RecordUpdate`, `BinOp::Range`). Records remain immutable values — an "update" expression constructs a new record; it does not mutate the base.

Record types can be named with a record type declaration or abbreviated with a type alias (Section 7.3):

```nulang
type Person = { name: String, age: Int, active: Bool }
type Point = { x: Float, y: Float }

fn translate(p: Point, dx: Float, dy: Float) -> Point {
  { x: p.x + dx, y: p.y + dy }
}
```

## 3.4 Sum Types

Sum types represent values that can be one of several alternatives. Nulang provides two sum type constructors: variants (tagged unions) and enums.

### 3.4.1 Variant Types

A variant type is defined with the `type` keyword and consists of a set of constructors, each optionally carrying a single payload type:

```nulang
type Option[T] =
  | Some(T)
  | None

type Result[T, E] =
  | Ok(T)
  | Error(E)

type Tree[T] =
  | Leaf
  | Node((Tree[T], T, Tree[T]))
```

The leading `|` on the first constructor is optional, and constructors may be written on one line (`type Color = Red | Green | Blue`). A constructor payload is a single parenthesized type; a constructor carrying several values takes a tuple payload, as `Node` shows. Record-style constructors with named fields (`Node { left: ..., ... }`) are **Planned**.

Nulang has no prelude: `Option` and `Result` are not built in. Programs declare the variants they need (as above) and then use the constructors as ordinary uppercase values. Variant constructors create values, and pattern matching destructures them:

```nulang
type Result[T, E] = Ok(T) | Error(E)

fn safe_divide(a: Float, b: Float) -> Result[Float, String] {
  if b == 0.0 then
    Error("Division by zero")
  else
    Ok(a / b)
}

fn describe(r: Result[Float, String]) -> String {
  match r with {
    | Ok(value) => "ok"
    | Error(msg) => msg
  }
}
```

> **Implementation status.** Declared variants work end-to-end: constructors create values, constructor names are first-class values (a payload constructor used as a value, such as `let f = Some in f(1)`, behaves as a one-argument function), and `match` destructures them with payload binding. At runtime a payload-less constructor is the bare tag string and a payload-carrying constructor is a record `{ ctor: <name>, payload: <value> }`; matching string-compares the tag. Nested constructor patterns match structurally — `Some(Some(x))` tests both tags and rejects an inner `None`. One limitation remains: tuple patterns do not check arity — a tuple-pattern arm tests only the positions it names, so `(a, b)` also matches a longer tuple (extra elements are ignored) and a position beyond the scrutinee's length binds nil (Section 6.7).

### 3.4.2 Enums

An enum is a variant type where no constructor carries data. Enums use the same concise syntax:

```nulang
type Color = Red | Green | Blue

type Status = Pending | Running | Completed | Failed
```

Enum constructors are pattern-matched like other variants:

```nulang
type Status = Pending | Running | Completed | Failed

fn status_message(s: Status) -> String {
  match s {
    | Pending   => "Waiting to start..."
    | Running   => "In progress..."
    | Completed => "Done!"
    | Failed    => "Something went wrong."
  }
}
```

## 3.5 Function Types

Function types describe the type of a function, including its parameter type, return type, effect row, and capability. The general form is:

```nulang
// fragment
A -> R ! {EffectRow} : cap
```

where `A` is the parameter type (a multi-argument function takes a tuple parameter `(A1, A2, ..., An)`), `R` is the return type, `! {EffectRow}` is the optional effect row, and `: cap` is the optional capability. Both annotations are omitted when the function is pure with the default `ref` capability:

```nulang
// Pure function: no effects
fn add(a: Int, b: Int) -> Int { a + b }

// Effectful function: the ! {IO} row is enforced — the body may perform
// IO effects and nothing else.
fn greet(name: String) -> Unit ! {IO} {
  handle perform IO.println(name) {
    | IO.println(msg) => unit
  }
}
```

In a *type* position the row is written the same way, e.g. `(String) -> Unit ! {IO}`. An empty effect row `{}` denotes a pure function; a row with a row variable `{IO, | e}` is effect-polymorphic (Section 4.5). Inside `fn` and `behavior` bodies the declared row is checked; unannotated bodies have their rows inferred.

Functions are first-class values: they can be passed as arguments, returned from other functions, and stored in data structures. Anonymous functions are written with `fn` (Section 6.8):

```nulang
fn compose[A, B, C](f: (B) -> C, g: (A) -> B) -> (A) -> C {
  fn(x) { f(g(x)) }
}

fn twice(f: (Int) -> Int) -> (Int) -> Int {
  fn(x) { f(f(x)) }
}
```

## 3.6 Generic Types

Type constructors and functions may be parameterized by type variables, enabling generic programming.

### 3.6.1 Type Parameters

Type parameters are declared in square brackets after a type or function name:

```nulang
type Pair[A, B] = { first: A, second: B }

fn first[T, U](p: (T, U)) -> T {
  match p with { | (a, _) => a }
}

type Box[T] =
  | Empty
  | Full(T)

fn map_box[A, B](b: Box[A], f: (A) -> B) -> Box[B] {
  match b with {
    | Empty   => Empty
    | Full(x) => Full(f(x))
  }
}
```

Unannotated parameters are inferred, so explicitly polymorphic functions can also be written without a parameter list:

```nulang
fn id(x) { x }        // inferred as forall 't. 't -> 't
fn const(a, b) { a }
```

### 3.6.2 Type Parameter Constraints

**Planned.** Typeclass constraints such as `[T: Ordered]` and the typeclasses `Eq`, `Ordered`, `Numeric`, `Show`, `Semigroup`, and `Monoid` are not implemented. Generic functions today are parametric: a polymorphic function may only use operations available for every type (construction, pattern matching, and passing values around).

### 3.6.3 Higher-Kinded Types

**Planned.** Type parameters that are themselves type constructors (`[F: Functor, A, B]`) are not implemented; every type parameter has kind `*`.

## 3.7 Array and String Types

### 3.7.1 Arrays

Arrays are homogeneous sequences with O(1) indexed access. The type of an array of `T` is written `[T]`. Array literals use square brackets and the element type is inferred:

```nulang
let numbers = [1, 2, 3, 4, 5] in
let first = numbers[0] in     // 1
first
```

Elements are accessed using bracket indexing: `arr[i]`. All elements must have the same type. Arrays are heap-allocated values; the `for x in arr body` comprehension (Section 6.15) iterates over them. Higher-level operations (`Array.map`, `Array.filter`, `Array.length`, …) are **Planned** with the standard library (Chapter 14).

### 3.7.2 Strings

The type `String` represents a sequence of characters. Strings are immutable values and support equality comparison (`==`). Concatenation (`++`), slicing, case conversion, and the other `String` module functions are **Planned** with the standard library.

String interpolation (`"Welcome to {name}!"`) is likewise **Planned**: the current lexer treats `{` and `}` inside string literals as ordinary characters. As noted in Section 2.5.3, `{input}` templates passed to `Pipeline.stage` are expanded at runtime by the pipeline builtin, not by the language.

## 3.8 Reference Types

Nulang's reference type system is adapted from Pony's capability system. Every reference to an object carries a *reference capability* that determines how it may be read, written, and shared. Capabilities are checked entirely at compile time and **erased at runtime** — the virtual machine does not re-check them (there are no capability opcodes; `mir::RValue::CapabilityCheck` compiles to `Const1`, i.e. `true`, in `src/mir_codegen.rs`).

### 3.8.1 The Seven Reference Capabilities

| Capability | Read | Write | Sendable | Description |
|------------|------|-------|----------|-------------|
| `lineariso` | Yes | Yes | Yes | Linear unique reference; must be consumed exactly once |
| `iso` | Yes | Yes | Yes | Unique reference; no aliases exist |
| `trn` | Yes | Yes | No | Transitioning; will become `val` |
| `ref` | Yes | Yes | No | Standard read-write reference |
| `val` | Yes | No | Yes | Immutable, globally readable |
| `box` | Yes | No | No | Read-only view of `ref` or `val` |
| `tag` | No | No | Yes | Opaque reference; only identity |

A capability-qualified reference type is written with an ampersand followed by the capability: `&iso String`, `&val [Int]`, `&ref Tree[T]`. An expression can be annotated with a capability directly using the contextual keyword `cap`: `expr :cap iso`.

```nulang
// ref: local mutable reference — `&` creates a ref-capability reference
let counter: &ref Int = &0

// every capability has a value-level constructor: `&cap expr` (see §3.9)
fn takes_iso(x: &iso Int) -> Int { *x }
let unique: &iso Int = &iso 5

// capability annotation on an expression (`:cap`)
let data = 5
let boxed = data :cap box
```

### 3.8.2 Capability Semantics

An `iso` reference guarantees that no other reference to the same object exists. This makes `iso` references safe to send to other actors, because the sender cannot retain any alias that would allow concurrent mutation. A `lineariso` reference is a stricter `iso` that must be consumed *exactly once*; the compiler tracks it with exactly-once linearity and reports a `LinearTypeError` if it is dropped or used twice.

A `val` reference is an immutable view of an object. Multiple `val` aliases can exist, and `val` references can be sent to other actors because the data cannot change.

A `ref` reference is the default for local mutable data. It allows reading and writing but cannot be sent to other actors because the sender could retain an alias and modify the data concurrently.

A `box` reference provides read-only access. It can view either a `ref` object (in which case the `ref` owner may still mutate it) or a `val` object (which is immutable). Because `box` does not guarantee immutability of the underlying object, it cannot be sent to other actors.

A `tag` reference is an opaque identifier. It carries no read or write permissions but can be used for identity comparison and can be sent to other actors. `tag` references are useful for maintaining relationships between actors without accessing their data.

### 3.8.3 Capability Defaults

When no capability is explicitly specified, the checker assigns defaults by context rather than inferring the most restrictive capability from usage:

- Function and behavior parameters without an explicit capability are bound at `ref` by the type checker.
- Literals and freshly constructed values default to `val`.
- The default capability context is `val`.
- Actor references produced by `spawn` carry `iso`.

Explicit annotations (`x: &iso T`, or the `! ... : cap` suffix on a function signature) override these defaults.

```nulang
actor Processor {
  // 'data' has the declared type String with the default parameter capability
  behavior process(data: String) {
    perform IO.print("Processing")
  }
}
```

## 3.9 Capability-Qualified Types

A capability-qualified type combines a reference capability with a structural type. This combination determines both what operations are permitted on the reference and what type-level guarantees hold about the data.

Capability-qualified types are written with the capability after an ampersand prefix:

```nulang
// A ref reference to a local value — `&` creates `ref` references
let counter: &ref Int = &0

// Value-level constructors exist for every capability: `&cap expr`
// builds a reference with that capability
fn takes_iso(x: &iso [Int]) -> [Int] { *x }  // pass `&iso data` at the call site
let unique: &iso [Int] = &iso [1, 2, 3]     // unique ownership — operand moved
let frozen: &val [Int] = &val [1, 2, 3]     // immutable shared view
let writable: &trn Int = &trn 0              // unique writer — operand moved
let read_only: &box Int = &box 0             // read-only view of anything
let identity: &tag Int = &tag 0              // opaque identity, no dereference

// A ref reference to a record value
let record_ref: &ref { count: Int, name: String } = &{ count: 0, name: "default" }
```

`&cap expr` is a value-level constructor for every capability; bare `&expr`
defaults to `&ref` (backward compatible). The unique constructors
(`&lineariso`, `&linear`, `&iso`, `&trn`) move the operand: a bare-variable
operand is consumed exactly like `consume x`, so a second `&iso x` on the
same binding is rejected. The shared constructors (`&ref`, `&val`, `&box`,
`&tag`) alias without consuming. Capabilities are erased before execution —
at runtime every constructor is a plain value move.

The compiler checks that all operations on a capability-qualified type are permitted. Attempting to write through a `val` reference or send a `ref` reference to another actor results in a compile-time error.

### 3.9.1 Capability Subtyping

Reference capabilities form a subtyping lattice, checked by the compiler's `is_subtype_of`. The key subtyping relationships are:

- `lineariso <: iso` — a linear unique reference can be used as a plain unique reference
- `iso <: trn` — a unique reference can downgrade to transitioning
- `trn <: ref` — a transitioning reference can be viewed as a mutable reference
- `ref <: box` — a mutable reference can be viewed as read-only
- `val <: box` — an immutable reference can be viewed as read-only
- `ref <: tag` — a mutable reference can be reduced to opaque identity
- `val <: tag` — an immutable reference can be reduced to opaque identity
- `box <: tag` — a read-only view can be reduced to opaque identity

These relationships enable safe capability transitions. For example, a function that accepts a `box` parameter can be called with either a `ref` or a `val` argument.

### 3.9.2 Recover Expressions

`recover` is a keyword in the current implementation (`src/lexer.rs`), and the `recover { body }` expression is implemented (Experimental). Its current meaning is narrower than Pony's: it is an **isolated scope with a sendability check** — the body is evaluated normally, and the capability checker requires the body's result to be sendable (`iso`, `val`, or `tag`; `src/effect_checker.rs` reports "recover body must evaluate to a sendable value" otherwise). The typechecker infers the body's type unchanged (`src/typechecker.rs`), and lowering is transparent (`src/hir_lower.rs`): `recover` does not wrap the result in `Ok`/`Error` and does not itself change any capability.

The Pony-style capability *upgrade* — constructing an `iso` or `val` reference from an expression that internally uses only `iso`, `trn`, `val`, or `tag` references, so the result's capability is stronger than its free variables would normally permit — is **reserved as future surface**. That semantics requires tracking the capabilities of all captured free variables, which the current implementation does not do. The planned form:

```nulang
// fragment — Planned capability-upgrade semantics not yet implemented
let immutable_tree: &val Tree[Int] = recover {
  Tree.Node { left: Tree.Leaf, value: 42, right: Tree.Leaf }
}
```

---

# Chapter 4: Effects

## 4.1 Effect System Overview

Nulang uses algebraic effects and handlers as the primary mechanism for defining, composing, and handling computational effects. Effects include IO, file system access, network communication, random number generation, time access, exceptions, state, and—uniquely—LLM inference. Every effectful operation in Nulang is expressed through this uniform mechanism.

The effect system has four key properties:

**Explicit effect tracking.** Every function's type includes an effect row that enumerates the effects it may perform, introduced by `!` in the signature. This makes dependencies on external systems visible in the type signature.

**Compositional handlers.** Effects are handled locally, not globally. A handler intercepts effect operations and defines their meaning in a specific context. Different handlers can provide different interpretations of the same effect.

**Resume-based semantics.** When an effect is performed, the current computation is suspended and its continuation is captured by the VM. The handler clause computes a value that becomes the result of the `perform` expression, after which the suspended computation resumes (see §4.4.2 for the current single-resumption model).

**Type-safe effect polymorphism.** Higher-order functions can be polymorphic in their effect rows via open rows (`{IO, | row}`), enabling generic code that works with both pure and effectful functions.

## 4.2 Effect Declarations

Effects are declared with the `effect` keyword, followed by a name and a set of operations. Each operation is written `name: (A, B) -> R` — a colon, the argument types in parentheses (or a single bare type for one argument, or nothing for zero arguments), an arrow, and the return type:

```nulang
effect Console {
  print: (String) -> Unit
  read_line: () -> String
}

effect FileSystem {
  read: (String) -> String
  write: (String, String) -> Unit
  exists: (String) -> Bool
}

effect Random {
  int: () -> Int
  float: () -> Float
}
```

Each operation has a name, a list of parameter types, and a return type. Effect declarations describe the interface; every `perform` is dispatched dynamically to whichever handler is innermost at runtime (§4.8).

## 4.3 Performing Effects

The `perform` keyword invokes an effect operation:

> **Note:** In the examples below, `Console` is a user-declared effect name used for illustration. The built-in I/O effect is `IO` (`IO.print`, `IO.println`, `IO.read`), which is handled by the runtime without requiring an explicit `handle` expression. The `Console` examples demonstrate how to declare, perform, and handle custom effects — the same patterns apply to any user-defined effect.

```nulang
fn greet_user() -> Unit ! {Console} {
  perform Console.print("What is your name?")
  let name = perform Console.read_line() in
  perform Console.print("Hello!")
}
```

The effect row `{Console}` in the function type indicates that `greet_user` may perform the `Console` effect. If a function performs effects beyond its declared row, the effect checker reports a compile-time `EffectError`. Performing an effect with no matching handler installed is a **runtime** error (`EffectError: Unhandled effect: 'Console'`), not a compile-time one — handlers are resolved dynamically.

Three operations are backed directly by the runtime when no source handler intercepts them: `LLM.ask(prompt)` (routes to the agent runtime's LLM client), `Signal.wait(name)` (workflow signal waiting), and `Timer.sleep(ms)` (used by workflow steps). All other effects must be handled by an enclosing `handle` expression.

## 4.4 Effect Handlers

The `handle` expression installs an effect handler around a body expression. The body follows `handle` directly, and the handler clauses are listed between braces. The keyword `with` between the body and the handler block is **optional** — `handle body { ... }` and `handle body with { ... }` are both accepted (the parser consumes an optional `with`, `src/parser.rs` `parse_handle`). `with` also allows naming a previously declared `handler` (`handle body with my_handler`), which installs the named handler's clauses:

```nulang
let result = handle {
  perform Console.print("Hello from handled code!")
  42
} with {
  | Console.print(msg) => 42
}
```

Each clause pattern matches on an effect operation, binds the operation's arguments to its parameter names, and evaluates its body. The clause body's final value is used to resume the suspended computation (§4.4.2). An optional `resume` keyword may appear before the `=>` (`| Op(x) resume => ...`); it is recorded in the AST for forward compatibility but does not change current behavior.

### 4.4.1 Handler Return Value

If the handled body completes without performing any effect that escapes, the value of the `handle` expression is the body's final value. If a performed effect is handled, the suspended body is resumed and, when it eventually completes, its final value is the result.

```nulang
let sum = handle {
  let x = perform Random.int() in
  let y = perform Random.int() in
  x + y
} {
  | Random.int() => 5
}
// sum == 10
```

### 4.4.2 Resume Semantics

When a `perform` is intercepted, the VM captures the suspended computation as a continuation and jumps to the matching handler clause. The clause body's final value becomes the resume value: the VM restores the continuation with that value as the result of the `perform` expression, and execution continues from the point of the `perform`.

The current implementation supports exactly this single-resumption model:

- **Once:** every handler clause ends by resuming with its body's value — this is the only supported mode.
- **Zero times (abort):** not directly expressible; `resume` without a captured continuation is a VM error. (Planned.)
- **Multiple times:** non-deterministic/backtracking handlers are not expressible. (Planned.)

```nulang
// Log-and-continue handler: prints, then resumes with unit
fn run_logged() -> Unit ! {Console, IO} {
  handle {
    perform Console.print("event happened")
  } {
    | Console.print(msg) => perform IO.print("logged")
  }
}
```

## 4.5 Effect Rows

Effect rows describe the set of effects a function may perform. They support polymorphism and subtyping, and appear in signatures after `!`.

### 4.5.1 Closed Effect Rows

A closed effect row enumerates exactly the effects a function performs, in braces. Effect rows always use braces; a bare `! Name` after the return type is the typed-error surface (`! Type`), never a row shorthand — the two are disambiguated by the braces:

```nulang
fn pure_function(x: Int) -> Int { x + 1 }

fn console_function(msg: String) -> Unit ! {Console} {
  perform Console.print(msg)
}

fn multi_effect() -> Unit ! {Console, FileSystem, Rand} {
  perform Console.print("Starting...")
}

// single-effect row (braces required — the bare `! IO` form is not accepted)
fn log_once(msg: String) -> Unit ! {IO} {
  perform IO.print(msg)
}
```

### 4.5.2 Open Effect Rows

An open effect row lists concrete effects followed by a row variable after a pipe, `{IO, | row}`, indicating polymorphism over any additional effects:

```nulang
// f may perform arbitrary effects; map_option passes them through
fn map_option[A, B](opt: Option[A], f: (A) -> B ! {| row}) -> Option[B] ! {| row} {
  match opt {
    | None => None
    | Some(a) => Some(f(a))
  }
}
```

The function `map_option` preserves the effect row of its callback function `f`. If `f` is pure, `map_option` is pure. If `f` performs `Console` effects, so does `map_option`.

### 4.5.3 Effect Row Subtyping

A function whose inferred effect row is a subset of the expected row can be used where that row is expected. The checker is deliberately conservative: a closed row is a subtype of another row only if all of its effects are listed; an open row on the expected side may absorb additional effects through its row variable, while an open row on the actual side is assumed to possibly contain any unlisted effect.

When a function or lambda carries an explicit `! {Row}` annotation, the effect checker infers the body's row and verifies it is a subset of the annotation, reporting an `EffectError` naming the offending effects otherwise.

## 4.6 Built-in Effects

The following effect names are recognized by the compiler without an
explicit `effect` declaration (any other name becomes a user-defined
effect, which should be declared with `effect`). This table is
illustrative, not exhaustive — `src/stdlib.rs` (surfaced via `nulang
--doc`) is the code-derived, authoritative inventory; it currently lists
substantially more (`Array`, `String`, `Int`, `Float`, `Debug`, `Env`,
`Process`, `System`, `Otp`, ...) than fit usefully in prose here.

**Corrected 2026-08-02** (found while writing conformance coverage for
this table — every row below was verified against the real compiled
binary, not assumed from the table that used to be here):

| Effect | Description | Status |
|--------|-------------|--------|
| `IO` | Console input/output | Implemented |
| `Http` | Network communication (`Http.get`/`post`/`serve`) | Implemented — this table previously said `Net`, which is not a recognized effect name; `perform Net.get(...)` fails with "Unhandled effect" |
| `FS` | File system access | Implemented |
| `Random` | Random number generation | Implemented — this table previously said `Rand`; that name type-checks (it's in the effect-name grammar) but has no runtime dispatch and fails with "Unhandled effect" at `perform` time |
| `Time` | Wall-clock read (`Time.now`) | Implemented |
| `Timer` | Delays (`Timer.sleep`/`Timer.after`) | Partial — a silent no-op (returns `unit` without sleeping) outside a workflow/actor context; real delays need the workflow runtime |
| `Signal` | Cross-actor coordination (`Signal.wait`) | Partial — `Signal.wait` is a silent no-op outside a workflow context; `Signal.notify` is not wired at all despite appearing in editor autocomplete |
| `Inference` | Inference provider (was `LLM`; `LLM.ask`/`Inference.ask` are runtime-backed) | Implemented |
| `LLM` | Deprecated alias for `Inference` | Implemented (emits an RFC 0010 deprecation warning on `perform`) |
| `STM` | Software transactional memory | **Not implemented** — `perform STM.*` always fails with "Unhandled effect" |
| `Async` | Asynchronous computation | **Not implemented** as a general user-facing effect — `perform Async.*` always fails with "Unhandled effect"; the only asynchronous dispatch path in the VM is internal, specific to `Inference.ask`/`LLM.ask` |
| `Cost` | Cost accounting | **Not implemented** — `perform Cost.*` always fails with "Unhandled effect" |

`Spawn`, `Send`, `Receive`, and `Migrate` were previously listed in this
table as effect names, which is misleading: they are language syntax
(`spawn`/`send`/`receive` keywords, §6.14 and Chapter 8; `migrate` is a
dedicated bytecode op with no `perform` surface), not effects invoked via
`perform Effect.op(...)`. `perform Spawn.spawn(...)` and similarly for
the other three are parse errors, not effect dispatch.

## 4.7 Effect Inference

The compiler infers effect rows automatically. A function's effect row is the union of all effects performed in its body, plus the effects of any functions it calls.

```nulang
// fragment — effect-row inference examples; FS.read needs capability file
// Effect row inferred as {IO}
fn inferred() {
  perform IO.print("Hello")
}

// Effect row inferred as {FS, IO}
fn read_and_print(path: String) {
  let content = perform FS.read(path) in
  perform IO.print(content)
}
```

Effect annotations are optional; when present (`! {Row}`), the inferred row must be a subset of the annotation (§4.5.3).

## 4.8 Effect Elaboration

Effects are not compiled to continuation-passing style. Lowering emits four dedicated opcodes — `Handle` (push a handler frame), `Perform` (search the handler stack, capture a continuation, jump to the clause), `Resume` (restore the captured continuation with the clause's value), and `Unwind` (pop the handler frame). Dispatch is therefore dynamic and resolved at runtime against the VM's handler stack; the static effect row is checked before compilation and imposes no runtime cost.

## 4.9 Effect Safety

The effect system guarantees several safety properties:

**Effect containment.** An effect performed inside a handler is intercepted by the innermost matching handler frame; effects escape only when no enclosing handler matches.

**No implicit effects.** Functions whose inferred row is empty perform no effects; any `perform` in their body would appear in the inferred row. Annotating such a function with an empty or narrower row than it needs is a compile-time `EffectError`.

**Runtime handler resolution.** Whether an enclosing handler exists for a performed effect is decided dynamically. An unhandled effect is a runtime `EffectError` (`Unhandled effect: 'Name'`), so programs that perform effects must install handlers (or use the runtime-backed operations) to avoid failing at runtime. Static exhaustiveness checking of handlers is planned.

---

# Chapter 5: Capabilities

> **Longevity Note (Erasable Vocabulary):** The capability *names*
> (`iso`, `trn`, `ref`, `val`, `box`, `tag`, `lineariso`) are part of the
> Frozen Core syntax. However, their *semantics* are defined purely by the
> capability lattice (subtyping and sendability). This guarantees that
> while the vocabulary is eternal, its interpretation can be safely retargeted
> by future language versions without breaking existing source syntax. A
> program written using `lineariso` today remains syntactically valid in 2226,
> even if the underlying compiler implementation of linear types evolves.

## 5.1 Capability System Overview

Nulang employs two complementary capability systems that together provide comprehensive security and safety guarantees:

1. **Reference capabilities** control how data can be read, written, and shared across actor boundaries. They are part of the type system and are checked at compile time. **Implemented** — and erased at runtime (no capability opcodes exist; capability checks compile to `Const1` in `src/mir_codegen.rs`).

2. **Authority capabilities** control what effects an actor can perform. They are declared on actors and checked both at compile time and runtime. **Planned** — the `capability` keyword does not exist in the current implementation; effect authority is currently expressed only through effect rows (Chapter 4).

These systems work together: reference capabilities prevent data races, while authority capabilities prevent unauthorized access to external resources.

## 5.2 Reference Capabilities

Reference capabilities are described in detail in Section 3.8. They form a lattice of permissions:

```
lineariso <: iso <: trn <: ref <: box <: tag
                              val <: box,  val <: tag
```

- `lineariso` — read+write, sendable, must be consumed exactly once
- `iso` — read+write, sendable
- `trn` — read+write, local
- `ref` — read+write, local
- `box` — read-only, local
- `val` — read-only, sendable
- `tag` — no access, sendable

The compiler uses these capabilities to guarantee:

**Memory safety.** No dangling references or use-after-free errors.

**Data-race freedom.** Mutable references cannot be shared between actors concurrently.

**Safe concurrency.** Only `lineariso`, `iso`, `val`, and `tag` references can be sent between actors; the capability analyzer rejects `send` arguments of any other capability with a compile-time `CapError`.

## 5.3 Authority Capabilities — Planned

*The `capability` declaration described here is not yet implemented — `capability` is not a keyword in the current lexer. The design follows.*

Authority capabilities are declared on actors using the `capability` keyword:

```nulang
// Planned — not yet implemented
actor FileProcessor {
  capability file
  capability io

  behavior process(path: String) {
    let content = perform FS.read(path) in
    perform IO.print("Read")
  }
}
```

Without the `capability file` declaration, the `perform FS.read(...)` would be a compile-time error. This is authority-based security: the actor must explicitly declare what external resources it needs.

## 5.4 Capability Delegation — Planned

*Not yet implemented (depends on §5.3). The design follows.*

Authority capabilities can be delegated from one actor to another:

```nulang
// Planned — not yet implemented
actor Supervisor {
  capability llm
  capability http

  behavior spawn_worker() {
    // Delegate capabilities to child actor
    spawn Worker with capabilities [llm, http]
  }
}
```

Delegation creates a capability chain that can be audited. The runtime tracks which actor granted which capability to which other actor.

## 5.5 Capability Revocation — Planned

*Not yet implemented (depends on §5.3). The design follows.*

Capabilities can be revoked at any time:

```nulang
// Planned — not yet implemented
actor ResourceManager {
  capability network

  behavior revoke_access(worker) {
    revoke worker.network
  }
}
```

After revocation, any attempt by the worker to perform network operations results in a runtime error.

## 5.6 Capability Auditing — Planned

*Not yet implemented (depends on §5.3). The design follows.*

The runtime maintains a capability graph that records all capability grants and revocations. This graph can be queried for security auditing.

## 5.7 Sendable Types

A type is *sendable* if it can be safely passed between actors. The sendable types are:

- Primitive types (`Bool`, `Int`, `Float`, `String`, `Unit`, `Nil`)
- `lineariso` reference types
- `iso` reference types
- `val` reference types
- `tag` reference types
- Immutable collections of sendable types
- Actor references (`Address`, as `tag`)

The compiler checks that all values passed as `send` arguments are sendable, rejecting anything else with a `CapError` at compile time.

## 5.8 Capability Defaults

In the absence of explicit annotations, the compiler applies the following defaults:

- **Function and behavior parameters:** bound at `ref` by the type checker when no capability is declared
- **Literals and freshly constructed values:** `val`
- **Ambient default (capability context):** `val`
- **Actor references from `spawn`:** `iso`
- **Return values:** inferred from the expression

Explicit type annotations (`x: &iso T`), capability annotations (`x :cap box`), and signature suffixes (`! {Row} : iso`) override these defaults.

---

# Chapter 6: Expressions

## 6.1 Expression Overview

Nulang is an expression-oriented language: every construct produces a value. The value of the last expression in a block is the value of the block. (Assignments, loops, and message sends evaluate to `unit`.)

## 6.2 Literals

Literal expressions produce constant values:

```nulang
// fragment
42          // Int literal
0x2A        // Int literal (hexadecimal)
3.14        // Float literal
"hello"     // String literal
true        // Bool literal
nil         // Nil literal (absence of a value)
()          // Unit literal (also written `unit`)
```

There is no character literal — characters are represented as single-character strings. (`Char` is planned; see §3.2.5.)

## 6.3 Variables

Variable references look up the value bound to a name:

```nulang
let x = 42 in
x  // evaluates to 42
```

Inside an actor behavior, actor state fields are accessed through `self` (`self.count`); bare field names are not in scope.

## 6.4 Function Application

Function application applies a function to its arguments. Functions are called positionally; paths through modules use dot syntax:

```nulang
// fragment
add(1, 2)              // function call
Math.Utils.clamp(v, 0, 10)  // call through a module path
list |> map(f)         // pipe operator (§6.9)
```

## 6.5 Let Bindings

The `let` expression binds a name to a value *in* a body expression — the `in` keyword is required:

```nulang
let x = 42 in
let y = x + 1 in
y  // evaluates to 43
```

An optional type annotation may follow the name: `let x: Int = 42 in ...`.

Recursive functions are bound with `let rec`:

```nulang
// Recursive local bindings are written as functions:
fn fact(n: Int) -> Int {
  if n <= 1 then 1 else n * fact(n - 1)
}
```

Let bindings are immutable. Mutable references are created explicitly with the prefix `&` operator and read back with the prefix `*` operator; assignment uses `=`:

```nulang
fn main() {
  let counter: &ref Int = &0
  counter = *counter + 1
  perform IO.print(*counter)  // 1
}
```

Mutable `var` bindings are planned for a future version (`var` is not currently a keyword).

## 6.6 Conditionals

The `if` expression chooses between branches. The `then` keyword is optional, branches may be single expressions or `{ }` blocks, and the `else` branch is optional (an omitted `else` yields `unit`):

```nulang
let x = 42
if x > 0 then
  "positive"
else
  "non-positive"

if x > 0 then {
  perform IO.print("positive")
}
```

When both branches are present they must have the same type.

## 6.7 Pattern Matching

The `match` expression performs pattern matching. The `with` keyword after the scrutinee is optional, and each arm may optionally begin with `case` or `|`:

```nulang
let option = Some(7)
match option {
  | Some(x) => x
  | None => 0
}
```

Supported patterns are: wildcard `_`, variable bindings, literals, tuples, records, variant constructors, and aliases (`name @ pattern`):

```nulang
type Tree[T] = Leaf | Node((Tree[T], T, Tree[T]))
let tree = Node((Leaf, 1, Leaf))
match tree {
  | Leaf => 0
  | n @ Node((l, v, r)) => v
}
```

Pattern guards (`| pat if cond => ...`) are implemented — the guard is a boolean expression evaluated with the pattern's bindings in scope after the pattern matches, and an arm whose guard fails falls through to the next arm (a guarded last arm whose guard fails raises the non-exhaustive-match error) — while list-cons patterns are planned for a future version. Nested variant, tuple, and record patterns all match structurally: sub-patterns are tested recursively against the payload, element, or field value, so `Some(Some(x))` rejects both `Some(None)` and `None`, and the `Node((l, v, r))` form above binds `l`, `v`, and `r`. One caveat remains: tuple patterns do not check arity — a pattern tests only the positions it names, so `(a, b)` also matches a longer tuple (extra elements are ignored) and a position beyond the scrutinee's length binds nil.

## 6.8 Lambda Expressions

Lambda expressions create anonymous functions with the `fn` keyword. An optional `->` may separate the parameter list from the body:

```nulang
let f1 = fn(x) { x + 1 }
let f2 = fn(x, y) { x + y }
let f3 = fn() { perform IO.print("hello") }
```

Parameter types and the effect row are inferred from use; lambdas may carry an effect annotation via their type ascription.

## 6.9 Pipe Operator

The pipe operator `|>` passes the left-hand side as the **first** argument to the right-hand-side call:

```nulang
// fragment
list |> map(f) |> filter(g) |> fold(h, 0)
// Equivalent to: fold(filter(map(list, f), g), h, 0)
```

## 6.10 Blocks

A block is a sequence of expressions enclosed in braces, separated by newlines or semicolons. The value of a block is the value of its last expression:

```nulang
let result = {
  let x = 1 in
  let y = 2 in
  x + y
} in result  // result == 3
```

## 6.11 Error Handling

Nulang has no exceptions. Recoverable errors are values, conventionally carried by user-declared `Result` and `Option` variants (there is no prelude — programs declare these types themselves):

```nulang
type Result[T, E] = Ok(T) | Error(E)

fn safe_divide(a: Float, b: Float) -> Result[Float, String] {
  if b == 0.0 then
    Error("Division by zero")
  else
    Ok(a / b)
}

match safe_divide(10.0, 2.0) {
  | Ok(result) => perform IO.print("ok")
  | Error(msg) => perform IO.print("error")
}
```

Unhandled effects and runtime faults (division-by-zero yields `nil`, failed `resume`, etc.) surface as runtime errors, not catchable exceptions.

## 6.12 Effect Handling

The `handle` expression installs an effect handler (see Chapter 4). The body follows `handle` directly; there is no `with` keyword:

```nulang
let result = handle {
  perform Console.print("Hello!")
  42
} {
  | Console.print(msg) => perform IO.print("logged")
}
```

## 6.13 Recover Expressions

`recover { body }` is implemented (Experimental) as an isolated scope whose result must be sendable; it performs no capability upgrade and no `Ok`/`Error` wrapping today. The Pony-style capability-recovery semantics are reserved for a future version. See §3.9.2.

## 6.14 Actor Operations

Actor expressions create and interact with actors:

```nulang
actor Counter {
  state count: Int = 0
  behavior increment(by: Int) { self.count = self.count + by }
  behavior get() { self.count }
}

let counter = spawn Counter { count = 0 }  // create an actor with initial state
send counter increment(1)                   // fire-and-forget message
counter ! increment(1)                      // infix form of send
let n = ask counter get()                   // request-response
n
```

Related expression forms:

- `receive { | Behavior(params) => expr }` — selective receive: scan the actor's own mailbox in FIFO order for the first message matching any arm, bind its payload values to the arm's params (missing values bind to nil, extras ignored), and evaluate the arm body; non-matching messages stay queued. Non-blocking: when nothing matches, the next message is popped and its first payload value yielded (nil when the mailbox is empty).
- `migrate actor_expr to node_expr` — move an actor to another node (§12.4).
- `for x in array body` — iterate over a built-in array; `break` exits early.
- `return expr` / `return` — early return from a function or behavior.
- `emit Event(args)` — emit an event (§10, §11).
- `self` — the current actor reference, and the receiver for state access (`self.field`).

---

# Chapter 7: Declarations

## 7.1 Declaration Overview

Declarations introduce names into the module scope. The supported declaration forms are: function definitions (`fn`), actor definitions (`actor`, `persistent actor`), agent definitions (`agent`), workflow definitions (`workflow`), type definitions (`type`, `type alias`), effect definitions (`effect`), foreign function declarations (`extern`), imports (`import`), and nested modules (`module`).

A top-level expression that is not a declaration is wrapped by the parser into a synthetic `__main` function, which is what the runtime executes.

## 7.2 Function Definitions

Functions are defined with the `fn` keyword. The full signature form is:

```nulang
// fragment
fn name[T, U](param: Type, ...) -> ReturnType ! {Effect, Row} : capability body
```

Every part after the parameter list is optional. Examples:

```nulang
fn add(a: Int, b: Int) -> Int {
  a + b
}

fn factorial(n: Int) -> Int {
  if n == 0 then 1 else n * factorial(n - 1)
}

fn log(msg: String) -> Unit ! {IO} {
  perform IO.print(msg)
}
```

Named functions may recurse by referring to their own name. Functions may be preceded by annotations; the only supported annotation is `@tool(description: "...")`, which exposes the function as an agent tool (§11.4):

```nulang
// fragment
@tool(description: "Search the knowledge base")
fn search(query: String) -> String {
  ...
}
```

## 7.3 Type Definitions

Type definitions create new types. A `type` declaration is either a record type (braces) or a variant type (pipe-separated constructors); each variant constructor carries at most one payload type (use a tuple payload for several fields):

```nulang
type Point = { x: Float, y: Float }

type Color = Red | Green | Blue

type Option[T] = None | Some(T)

type Result[T, E] = Ok(T) | Error(E)

type Tree[T] = Leaf | Node((Tree[T], T, Tree[T]))
```

`type alias` introduces a transparent alias for an existing type:

```nulang
type alias UserId = Int
type alias Handler[T] = (T) -> Unit
```

## 7.4 Actor Definitions

Actor definitions declare actor types (see Chapter 8 for full details):

```nulang
actor Counter {
  state local count: Int = 0

  behavior increment() {
    self.count = self.count + 1
  }

  behavior get() {
    self.count
  }
}
```

## 7.5 Effect Definitions

Effect definitions declare new effect types (see Chapter 4 for full details). Operations use the `name: (Arg, Types) -> Return` form:

```nulang
effect Console {
  print: (String) -> Unit
  read_line: () -> String
}
```

## 7.6 Imports

The `import` declaration brings a module path into scope. Only the plain path form is implemented:

```nulang
import stdlib::list
import stdlib::math
```

Selective imports (`import List exposing [map]`) and aliased imports (`import Math as M`) are planned. Imports are resolved at compile time and have no runtime cost.

## 7.7 Module Structure

A Nulang source file (conventionally `.nula`) is compiled as a single module named `main`. Named modules can be nested inside a file with `module Name { ... }`, which prefixes its declarations into the flat namespace:

```nulang
module Math {
  fn pi_val() -> Float { 3.14159 }

  fn circumference(radius: Float) -> Float {
    2.0 * pi_val() * radius
  }
}
```

Module-level visibility enforcement and multi-file compilation units are planned.

## 7.8 Generics in Declarations

Type parameters are declared in brackets after the function or type name. There are no constraints or bounds on type parameters (typeclass constraints are planned):

```nulang
type List[T] = Nil | Cons((T, List[T]))

fn map[A, B](list: List[A], f: (A) -> B) -> List[B] {
  match list {
    | Nil => Nil
    | Cons((head, tail)) => Cons((f(head), map(tail, f)))
  }
}

type Tree[T] = Leaf | Node((Tree[T], T, Tree[T]))
```

## 7.9 Visibility

All declarations are visible throughout the program; there is currently no visibility enforcement. The `pub` keyword is accepted before any declaration for forward compatibility, and `priv` is reserved as a keyword for the planned visibility system:

```nulang
pub fn exported(x: Int) -> Int { x + 1 }
```

## 7.10 Documentation

Documentation comments use the `///` line form (there is no block doc-comment form):

```nulang
/// Calculate the factorial of a non-negative integer.
/// Returns 1 for n = 0, and n * factorial(n - 1) otherwise.
fn factorial(n: Int) -> Int {
  if n == 0 then 1 else n * factorial(n - 1)
}
```

---

# Chapter 8: Actors

## 8.1 Actor Model Overview

Actors are the fundamental unit of concurrency and state in Nulang. An actor is an isolated computational entity with:

- A unique identity (actor ID)
- A mailbox for receiving messages
- Private state that cannot be accessed from outside
- A set of behaviors that process messages
- Optional supervision and lifecycle management

Actors communicate exclusively through asynchronous message passing. There is no shared mutable state between actors.

## 8.2 Actor Declaration

Actors are declared with the `actor` keyword. State fields are accessed through `self` inside behaviors — bare field names are not in scope:

```nulang
actor Counter {
  state local count: Int = 0

  behavior increment() {
    self.count = self.count + 1
  }

  behavior get() {
    self.count
  }

  behavior reset() {
    self.count = 0
  }
}
```

A persistent actor is declared by prefixing with `persistent` (Chapter 9):

```nulang
// fragment
persistent actor Account {
  state durable balance: Int = 0
  ...
}
```

## 8.2a Entity Declaration

An `entity` is a durable-first actor. It is equivalent to `persistent actor` with `event_sourced` as the default state model, and is the recommended surface for long-lived domain objects. State fields without an explicit model default to `event_sourced`; `local`, `durable`, and `crdt` may still be used explicitly.

```nulang
entity BankAccount {
  state balance: Int = 0          // event_sourced by default
  state local scratch: Int = 0  // ephemeral, explicitly marked

  behavior deposit(amount: Int) {
    self.balance = self.balance + amount
  }
}
```

`entity` desugars to `persistent actor` during parsing. The runtime treats an entity exactly like a persistent actor whose event-sourced fields are recovered from the event journal on restart. See Chapter 9 for persistence details and RFC 0005 for the full entity design.

## 8.3 State Declarations

State is declared with the `state` keyword:

```nulang
// fragment
state model name: Type = initial_value
```

The state model (`local`, `durable`, `event_sourced`, or `crdt`) determines how the state is stored and recovered (§9.3). The model is optional and defaults to `local`; the type annotation is optional and inferred from the initial value. The `= initial_value` initializer is required.

## 8.4 Behavior Declarations

Behaviors are declared with the `behavior` keyword. A behavior has parameters but **no declared return type**; its value is the value of its body expression. Optional `! {Row}` effect and `: capability` annotations may follow the parameter list (the capability defaults to `ref` when omitted):

```nulang
// fragment
behavior name(parameters) ! {Row} : capability {
  // body
}
```

Behaviors execute sequentially within an actor. Each behavior processes one message at a time. The value of a behavior that is invoked via `ask` becomes the reply.

## 8.5 Message Passing

Messages are sent with the `send` keyword or the infix `!` operator, naming the target behavior and its arguments:

```nulang
actor Counter {
  state count: Int = 0
  behavior increment() { self.count = self.count + 1 }
}

let counter = spawn Counter { count = 0 }
send counter increment()
counter ! increment()
```

Message sending is asynchronous and non-blocking: the message is enqueued in the target's mailbox and the sender continues immediately. Send arguments must be sendable (§5.7).

**Implementation status (verified 2026-08-02).** Behavior name resolution has two surprising edge cases, neither caught at compile time:
1. A `send`/`ask` naming a behavior the target actor doesn't declare does not drop the message or error -- it silently runs the actor's first declared behavior instead (`Runtime::send_message` resolves an unknown name to behavior id 0; see the doc comment on `send_message` in `src/runtime/mod.rs`).
2. Two different actor types that happen to declare a same-named behavior can collide: dispatch resolves by suffix-matching the *variable's* name hint, not the target's concrete actor type, so addressing one type's variable can run the *other* type's same-named behavior against the first type's state. See `conformance/behavior/lifecycle_03/04/05_*.nula` for the evidence trail. Both are tracked as known gaps, not fixed -- `send_message` is called pervasively enough that correcting the fallback needs a wider, carefully-audited change (the remote-message delivery path in §12.4 deliberately mirrors the same behavior-id-0 fallback today).

## 8.6 Request-Response

The `ask` expression sends a message and waits for the behavior's value as a response:

```nulang
actor Counter {
  state count: Int = 0
  behavior increment() { self.count = self.count + 1 }
  behavior get() { self.count }
}

let counter = spawn Counter { count = 0 }
send counter increment()
let count = ask counter get()
count
```

## 8.7 Actor Lifecycle

Actors are created with `spawn`, which takes the actor type name and a brace-enclosed list of initial state values (the braces are required, and may be empty to use the declared defaults):

```nulang
// fragment
let counter = spawn Counter { count = 0 } in
let other = spawn Counter {} in
...
```

`spawn` returns an actor reference with capability `iso`. Explicit actor shutdown (`stop`) is planned; today actors are stopped by the runtime when they fail or when their supervisor shuts them down (§8.8), and `exit` is a reserved keyword for the future lifecycle surface.

## 8.8 Supervision

Supervision is provided by the runtime, not by syntax. A spawned actor can be attached to a supervisor with a strategy (`OneForOne`, `OneForAll`, `RestForOne`, `SimpleOneForOne`) and a restart policy (`Permanent`, `Temporary`, `Transient`, `RespawnOnNodeLoss`); when a behavior raises a runtime error the supervisor applies its policy — restarting the actor (with state rebuilt from its persistence store, if any), shutting it down, or escalating. `RespawnOnNodeLoss` (RFC 0014, §12.6) additionally re-spawns a durable child on another node when its home node is confirmed gone. The stress tests exercise these paths under load. Declarative in-language supervision syntax is planned.

```nulang
// Supervised actors are spawned and wired through the runtime API;
// language-level supervision declarations are planned.
```

## 8.9 Actor Types

Actor declarations may be parameterized over types, like functions. State field initializers are required, so generic actors typically use an empty built-in array or a variant constructor as the default:

```nulang
type Option[T] = None | Some(T)

actor Queue[T] {
  state local items: [T] = []

  behavior size() {
    self.items
  }
}
```

## 8.10 Actor References

Actor references are values of the primitive type `Address` — opaque identifiers that can be passed between actors (capability `tag` once shared):

```nulang
actor Counter {
  state count: Int = 0
  behavior get() { self.count }
}
actor Worker {
  behavior use_counter(addr) { 0 }
}

let counter = spawn Counter { count = 0 } in
let worker = spawn Worker {} in
let addr = counter in
send worker use_counter(addr)
```

---

# Chapter 9: Persistent Actors

## 9.1 Overview

Persistent actors survive process restarts through automatic checkpointing, event journaling, and deterministic replay. They are the foundation for durable execution in Nulang.

**Implementation status.** Persistence is implemented in the runtime behind a `PersistenceStore` trait with three backends — in-memory, JSON file, and SQLite. `persistent actor` with `durable`/`event_sourced`/`crdt` state models parses and runs; checkpointing and journaling happen on each behavior step, and supervisors rebuild actor state from the store on restart. Snapshot compaction and the test-only replay helpers mentioned below are planned.

## 9.2 Declaring Persistent Actors

A persistent actor is declared with the `persistent` keyword:

```nulang
type Result[T, E] = Ok(T) | Error(E)

persistent actor BankAccount {
  state durable balance: Int = 0

  behavior deposit(amount: Int) {
    self.balance = self.balance + amount
  }

  behavior withdraw(amount: Int) {
    if amount > self.balance then
      Error("Insufficient funds")
    else {
      self.balance = self.balance - amount
      Ok(unit)
    }
  }

  behavior get_balance() {
    self.balance
  }
}
```

## 9.3 State Models

Persistent actors support four state models:

| Model | Persistence | Replication | Recovery |
|-------|------------|-------------|----------|
| `local` | None | None | Reset to initial value |
| `durable` | Snapshot + journal | None | Replay from journal |
| `event_sourced` | Event journal | Event stream | Full event replay |
| `crdt` | Delta log | Automatic merge | CRDT merge |

## 9.4 Automatic Checkpointing

The runtime automatically checkpoints persistent actor state after each behavior invocation:

1. Behavior completes successfully
2. Runtime captures state snapshot
3. Snapshot is written to persistent storage
4. Journal entry is recorded for replay

## 9.5 Event Journaling

All state mutations are recorded in an event journal:

```nulang
persistent actor ShoppingCart {
  state durable items_count: Int = 0
  state event_sourced events: Int = 0

  behavior add_item(item: Int) {
    self.items_count = self.items_count + 1
    emit ItemAdded(item)
  }

  behavior remove_item(item_id: Int) {
    self.items_count = self.items_count - 1
    emit ItemRemoved(item_id)
  }
}
```

`emit Name(args)` records an event in the journal; on recovery the actor's `event_sourced` state is reconstructed by replaying it.

## 9.6 Crash Recovery

Runtime recovery process:
1. Read last snapshot for each persistent actor
2. Replay events from journal from snapshot point forward
3. Restore `durable` state from snapshot
4. Reconstruct `event_sourced` state by applying replayed events
5. Merge `crdt` state from all available replicas

**Implementation status (verified 2026-08-02; apply-handler recovery
fixed 2026-08-13).** Recovery does not re-execute an `apply` handler
(they are inlined at each `emit` call site by `hir_lower.rs`); instead
`emit_event` persists the field's CURRENT value — captured AFTER the
inlined apply handler ran and after the unconditional "+1" every
`event_sourced` field gets per emit (`src/runtime/workflow.rs`) — in
the `EventEntry`, and `recover_actor` restores that exact post-apply
value from the event. The stored value, not a bare event count, is what
recovery reconstructs, so non-trivial `apply` handlers survive a crash
without needing an addressable bytecode unit. Pinned by
`test_event_sourced_apply_handler_recovery`
(`src/integration_tests/mod.rs`): an `entity Counter` with
`apply | Incremented(by) => self.count = self.count + by`, sent
`increment(3)` then `increment(4)` with no crash, reaches `count = 9`
(matches `conformance/behavior/persist_07_emit_accumulates_across_sends.nula`);
the same two messages with a crash-and-recover between them recover to
`count = 4` after the first message (apply's `0 + 3`, plus the +1 bump)
and reach `9` after continuing — identical to the never-crashed
baseline.

A related, more severe bug was found and fixed alongside this: before
this session, `recover_actor` never restored `Actor.state_models` (the
per-field Local/Durable/EventSourced/Crdt map) on the rebuilt actor,
so *every* field silently reverted to the `Local` default the moment
an actor recovered once -- a second crash would have dropped `durable`
fields from the snapshot entirely, and `event_sourced` fields would
have stopped accumulating altogether (the "+1 per emit" bump above
iterates `state_models` to find which fields to bump). Fixed by
restoring `state_models` from the recovery module's `actor_metadata`
in the same place `bytecode_module`/`bytecode_offsets` are restored.

## 9.7 Deterministic Replay

- Same input sequence + same initial state = same output
- Enables testing and debugging of persistent actor execution
- Dedicated `snapshot` / `replay_from_start` testing helpers are planned

## 9.8 Snapshotting and Compaction — Planned

- Snapshots capture state at a point in time
- Old snapshots and journal entries compacted periodically
- Configurable snapshot frequency and retention

## 9.9 Event Sourcing

- All state changes captured as immutable events
- Automatic event journal maintenance
- State derived by applying events

## 9.10 CRDT State

**Implementation status (verified 2026-08-15):** the concrete-type selector
and the `Crdt.*` effect module are both landed. `state crdt <name>: <Type>
= expr` accepts `gcounter | pncounter | gset | orset | aworset |
lwwregister | mvregister | rga` (`CrdtType::from_keyword`,
`src/parser.rs:1752-1809`); crdt fields register with
`CrdtManager::register_actor_field` (eagerly — the standalone runtime
initializes `crdt_manager`, so `state crdt` works without distribution) and
merge through `CrdtManager` on replication rounds. The `Crdt.*` effect
module (`perform Crdt.<op>("field", …)`) is the only mutation path, with
per-type operation sets enforced by
`CrdtManager::apply_field_op` (`src/runtime/crdt_manager.rs`):

| `crdt` type    | operations                            |
|----------------|---------------------------------------|
| `gcounter`     | `increment`, `read`                   |
| `pncounter`    | `increment`, `decrement`, `read`      |
| `gset`         | `add`, `read`                         |
| `orset`        | `add`, `remove`, `read`               |
| `aworset`      | `add`, `remove`, `read`               |
| `lwwregister`  | `set`, `read`                         |
| `mvregister`   | `set`, `read`                         |
| `rga`          | `read` (insert/delete not surfaced)   |

An op outside a field's set (e.g. `decrement` on a `gcounter`) returns nil
and mutates nothing; a raw `self.field = expr` assignment to a crdt field is
ignored at runtime so it cannot orphan `state_data` from the replicated
entry. `read` materializes the value back into `state_data`, so `self.field`
reads stay consistent. `.nula`-level conformance coverage lives in
`conformance/behavior/crdt_*.nula`.

**Recovery limitation:** `recover_actor` restores the materialized
`state_data` value and the `CrdtManager` entries from `crdt_snapshot`, but
does not rebuild `CrdtManager.field_map` (the `(actor_id, field_name) →
CrdtId` link is not persisted). On a recovered actor, `self.field` still
reads the materialized value, but `perform Crdt.*` is a silent nil no-op
until the field is re-registered. Pinned by
`test_crdt_field_survives_recovery` (a post-recovery `Crdt.increment`
leaves `state_data["count"]` unchanged).
---

# Chapter 10: Workflows

**Deprecated (RFC 0004, Draft).** `workflow` is on a path out of the
language surface entirely, toward an ordinary `actor` + Cloud SDK
(`nlc.workflow`) library pattern. The keyword remains functional today
(the CLI emits a deprecation warning on `workflow` declarations) and
this chapter still describes its current behavior, but treat everything
below as a snapshot of a feature being phased out, not a design target
to invest deeply against. `conformance/behavior/workflow_*.nula` pins
the current, verified behavior precisely — treat it as more current
than this prose for edge cases.

**Known issues, current implementation (verified 2026-08-02, not fixed
in this pass beyond the first):**
1. **Fixed this pass:** a workflow-only program (no other `actor`
   declaration) used to never activate the actor runtime at all —
   `spawn`/`send` were silent stubs, so every step was inert with no
   error. `src/main.rs`'s actor-detection now includes `Decl::Workflow`.
2. ~~Saga step compensation (§10.8) is index-based against the *whole
   module's* declaration order, not the workflow's own steps — an
   `actor` declared before the `workflow` in the same file silently
   shifts which step's compensation runs, with no error.~~ **Fixed
   2026-08-13:** compensation pairs now carry the step's ABSOLUTE
   module behavior index (`mir::Module::compensation_of`), so the
   codegen patches only the owning actor's steps. The same latent
   misindexing in step dispatch was fixed alongside: a workflow actor's
   `bytecode_offsets` are now compressed to ITS OWN behaviors (local
   step ids 0..step_count-1, matching `layout_workflow_behavior_table`)
   instead of the whole module's list — with a preceding actor, local
   step 0 previously ran the OTHER actor's first behavior. Pinned by
   `test_saga_compensation_ignores_non_workflow_actors` (compile-level
   `compensate_offset` placement + behavioral happy path and
   reverse-order compensation run with a pre-declared `actor Before`).
3. A workflow-level `compensate { }` block (as opposed to a per-step
   one) parses but is silently dropped during lowering and never runs.
4. ~~The single-argument `perform Timer.sleep(ms)` form suspends a step
   and never resumes it in a plain invocation (permanent hang, not an
   error) — only the two-argument durable form, `Timer.sleep(name, ms)`,
   works. `ms = 0` hangs too.~~ **Fixed 2026-08-13:** the timer-wheel
   wake now resumes the suspended `PerformAsync` via a full VM resume
   (`fire_timer_sleep_wake`), the re-executed opcode sees the fired flag
   and completes, and the workflow completion bookkeeping (step_index
   advance, `StepCompleted` event, checkpoint) runs exactly like the
   signal-wait/LLM resume paths. The resume distinguishes completion from
   re-suspension by the VM result, not `take_suspended_state` (which
   returns the completed frame state after a normal finish — a blind
   re-capture re-stalled the actor). Pinned by
   `test_workflow_timer_sleep_single_arg_resumes`.
5. ~~A step failing (e.g. a non-exhaustive match) produces no diagnostic
   at all — no stderr, exit 0 — only a difference in which
   compensations ran (if any) reveals it happened.~~ **Fixed
   2026-08-13:** a failed step now records a durable
   `WorkflowEvent::StepFailed { step_name, error }` alongside saga
   compensation, `Runtime::workflow_failures()` exposes them, and the
   CLI prints `workflow step 'X' failed: <error>` to stderr and exits
   nonzero instead of silently succeeding. Pinned by
   `test_workflow_step_failure_is_recorded_and_surfaced` and a CLI
   smoke run (`boom` step, exit 1).
6. `parallel` blocks (§10.6) run their branches sequentially today, not
   concurrently, though the observable contract (all branches complete;
   the aggregate step waits; a branch failure fails the whole block)
   holds — the desugaring's own comment already says concurrent
   synthesis isn't ported yet.
7. `fail` inside a step is sugar for `return`, not a failure signal —
   triggering real step failure requires a genuine runtime error.
8. Saga compensation only sees a step as "completed" if the step body
   manually increments `self.step_index`; nothing does this
   automatically for ordinary sequential steps despite the desugaring
   comment implying otherwise.

## 10.1 Workflows as Actor Graphs

- Workflows are long-running, durable compositions of actor interactions
- Special kind of persistent actor that orchestrates other actors
- Built from steps: individual units of durable work
- Survive crashes transparently; compensation for failures

## 10.2 Workflow Declaration

- Declared with the `workflow` keyword
- Body contains `step` declarations, `parallel` blocks, and an optional workflow-level `compensate` block — **not** state declarations (workflows carry no `state` fields in the current syntax)

```nulang
workflow OrderFulfillment {
  step receive_order {
    perform IO.print("Processing order")
  }

  step validate_inventory {
    perform IO.print("Checking stock")
  }

  step charge_payment {
    perform IO.print("Charging payment")
  }

  step ship_order {
    perform IO.print("Shipping")
  }
}
```

## 10.3 Steps

- A step is `step name { body }` — named, durably executed block of code
- Steps take **no parameters** and declare no return type in the current syntax; data is passed through actor state and messages
- Checkpointed before and after execution
- Can declare compensation logic (§10.8)
- Idempotent by design

## 10.4 Sequential Execution

- Default execution mode for workflow steps
- Steps run in declaration order

## 10.5 Conditional Execution

- Ordinary `if` expressions within step bodies
- Conditional routing of workflow based on step output

## 10.6 Parallel Execution

- The `parallel { step ... }` block declares steps that execute concurrently
- Waits for all branches before continuing
- Entire block fails if any branch fails

```nulang
workflow ParallelProcessing {
  parallel {
    step gather_a {
      perform IO.print("gathering A")
    }
    step gather_b {
      perform IO.print("gathering B")
    }
  }

  step aggregate_results {
    perform IO.print("aggregating")
  }
}
```

(Iteration with `for` over collections happens inside step bodies; a `parallel for` form is planned.)

## 10.7 Loops and Iteration

- `for x in array body` loops within workflow steps
- State accumulated across iterations
- Each iteration checkpointed for durability

## 10.8 Compensation and Sagas

- The `compensate { expr }` block declares undo logic, either per step (after the step body) or once at workflow level
- Saga pattern: automatically compensates on failure
- Compensation runs in reverse order of step execution

```nulang
workflow SagaTransaction {
  step reserve_inventory {
    perform IO.print("reserved")
  } compensate {
    perform IO.print("released")
  }

  step charge_payment {
    perform IO.print("charged")
  } compensate {
    perform IO.print("refunded")
  }

  step ship_goods {
    perform IO.print("shipped")
  }
}
```

## 10.9 Human-in-the-Loop — Planned

- `await_human` construct pauses workflow for human input (planned; not a keyword today)
- Configurable assignee, timeout, and default action
- Workflow state durably preserved during wait

The runtime-backed `Signal.wait(name)` operation (performed as `perform Signal.wait("approval")`) already lets a workflow step block until a named signal is delivered, which is the current foundation for human-in-the-loop patterns.

## 10.10 Time-Based Operations

- `perform Timer.sleep(ms)` is runtime-backed and available today for delays inside steps
- `sleep_until` and calendar-based scheduling are planned
- Workflow state preserved during sleep

## 10.11 Subworkflows — Planned

- `subworkflow` was freed as an identifier per RFC 0010 §C.6 (GOVERNANCE §2a); invoking one workflow from another is not yet wired into the parser
- Design: inherit parent's durability guarantees and participate in same compensation scope

## 10.12 Error Handling and Retry — Planned

- Automatic retry with configurable policies is planned (`retry` is not a keyword today)
- Exponential backoff, max attempts, transient error detection
- Integration with saga compensation for non-retryable failures — today, a failing step triggers the workflow's compensation chain (§10.8)

---

# Chapter 11: AI Runtime

## 11.1 Overview

- AI capabilities accessed through the `Inference` effect (`perform Inference.ask(prompt)`).
  The legacy name `LLM.ask` is a deprecated alias (RFC 0010) and emits a
  compiler warning.
- **Crate separation:** all pure AI types (clients, providers, memory,
  pipelines, debates, supervisor teams, usage tracking, tool schemas) live in
  the standalone `nulang-ai` workspace crate (`crates/nulang-ai/`) with **zero
  dependencies on the core language crate**. Core imports them directly
  through `use nulang_ai::…;` — there is no `src/ai/` façade module. The
  crate is optional, pulled in by the `ai-runtime` cargo feature.
- **Runtime integration:** `src/runtime/ai_impls.rs` provides the trait
  impls (`PipelineRuntime`, `DebateRuntime`, `SupervisorRuntime` for
  `Runtime`) that the orphan rule requires to live in core;
  `src/runtime/agent.rs` runs the agent LLM completion pipeline
  (`build_agent_llm_request`, `finish_agent_llm`, `complete_agent_llm`)
  against actor durable state; `src/runtime/llm.rs` owns the persistent
  `nulang-llm` worker thread and completion channels polled by the
  scheduler. These modules access `Runtime` internals and stay in core;
  they are the bridge between the pure AI types and the actor model.
- **No AI-specific opcodes.** The single `PerformAsync` opcode (`0xC6`)
  dispatches all AI effects (`Inference.ask`, `Pipeline.run`, etc.) through
  the generic effect mechanism. The monolithic AI opcode range (`LlmAsk`,
  `PipelineNew`…`DebateRun`, 0x9D–0xC5) has been removed.
- `agent`/`workflow`/`database` declarations are deprecated (RFC 0004);
  new code should use `actor` with Cloud SDK imports.
## 11.2 Agent Declarations

An `agent` declaration defines an LLM-backed actor. The `model` field is required; all other fields are optional:

```nulang
agent ResearchAssistant = {
  model: "gpt-4",
  system_prompt: "You are a research assistant. Provide structured reports.",
  tools: [search, summarize],
  memory: { max_turns: 50 },
  semantic_memory: { dimensions: 64 },
  procedural_memory: { namespace: "research" },
  pricing: { input: 30.0, output: 60.0 }
}
```

Spawning an agent creates an actor with built-in behaviors:

- `ask prompt` — send a prompt, receive the model's response (also reachable as `perform LLM.ask(...)`)
- `usage` — report token usage and cost (from `pricing`)
- `store_fact key value` / `recall key` — long-term memory access

## 11.3 Model Providers

- Unified interface for multiple providers
- OpenAI, Anthropic, Google, local models (Ollama/vLLM)
- The provider and model are selected by the agent's `model` string

Provider configuration files (`config llm { ... }`) are planned — `config` is not a keyword in the current implementation.

## 11.4 Tool System

- Nulang functions are exposed as agent tools with the `@tool(description: "...")` annotation
- An agent's `tools: [...]` list names the `@tool` functions it may invoke
- The runtime executes tool calls the model emits and feeds the results back

```nulang
// fragment
@tool(description: "Evaluate an arithmetic expression")
fn calculate(expression: String) -> Float {
  ...
}

agent CalculatorAgent = {
  model: "gpt-4",
  tools: [calculate]
}
```

(The standalone `tool name(params): Type { ... }` declaration form shown in earlier drafts is not implemented; tools are ordinary `fn`s carrying the `@tool` annotation.)

## 11.5 Memory (Short-term, Long-term, Event)

### 11.5.1 Short-term Memory
- Conversation context, bounded by `memory: { max_turns: N }` (default 50)
- Persists for duration of interaction

### 11.5.2 Long-term Memory
- Vector embeddings for semantic retrieval, configured by `semantic_memory: { dimensions: N }` (default 64)
- Store and recall facts based on semantic similarity via the `store_fact` / `recall` behaviors

### 11.5.3 Event Memory
- Procedural memory namespace via `procedural_memory: { namespace: "..." }`
- Immutable audit trail via event sourcing

## 11.6 Planning, Orchestration, and Delegation

Multi-agent orchestration is provided by three built-in modules whose call chains the compiler recognizes:

- `Pipeline.new(name).stage(name, agent).run(input)` — sequential agent pipeline
- `Supervisor.new(name).worker(agent).run(input)` — supervisor dispatching to worker agents
- `Debate.new(name).participant(agent).run(topic)` — multi-agent debate

Structured planning and delegation to specialist sub-agents beyond these builtins are planned.

## 11.7 Observability

- Token usage and cost tracking through the agent's `usage` behavior and the `pricing` config
- Automatic logging of LLM interactions
- OpenTelemetry export and prompt/response logging configuration are planned

---

# Chapter 12: Distributed Runtime

## 12.1 Overview

- Actor model extended across machine boundaries
- Location-transparent: same code on single node or cluster
- Message routing handled by runtime
- CRDT convergence, fault containment and recovery

**Implementation status.** The distributed runtime exists: a TCP wire protocol with a node handshake, gossip-based cluster membership, an address resolver with a remote-actor cache providing location transparency, and CRDT synchronization between nodes. Virtual actors (activation on demand by name) are planned.

## 12.2 Clustering

- Nodes connected through mesh network
- Discovery via gossip protocol or seed list
- Configurable heartbeat and gossip parameters

Cluster parameters are configured through the runtime API today; a declarative `config cluster { ... }` block is planned (`config` is not a keyword in the current implementation).

## 12.3 Node Lifecycle

- **Joining**: Connect to seeds, announce presence
- **Active**: Participate in routing and hosting
- **Leaving**: Drain actors, disconnect gracefully
- **Failed**: Detected via missed heartbeats

## 12.4 Message Routing

**Implementation status (verified 2026-08-14):** a `nulang node --listen
<ADDR> [--seed <ADDR>] [--expected-nodes <N>] [--tls-cert/--tls-key]`
CLI forms a real cluster (`main.rs:220` → `enable_distribution` +
`join_cluster`); no `.nula`-level `config cluster` block yet.

- The runtime resolves an actor's address (local or remote) and routes messages accordingly
- Remote-actor references are cached; the programmer sends messages and the runtime handles routing

```nulang
actor ShoppingCart {
  state items_count: Int = 0
  behavior add_item(n: Int) { self.items_count = self.items_count + n }
}

let cart = spawn ShoppingCart { items_count = 0 } in
send cart add_item(42)
// Runtime routes to whichever node hosts the actor
```

This example's `send` is ordinary single-node actor messaging (§8.5) —
it works today, but doesn't exercise any routing decision since there's
only ever one node to route to. `send remote <actor> <behavior>(args)`
and `ask remote <actor> <behavior>(args)` are also accepted syntax.
**Fixed 2026-08-13:** single-node (or distributed-disabled) remote
sends/asks now fall back to local delivery instead of silently
dropping — the distribution wrapper resolves a remote address to local
delivery when the node is local or the transport is unwired, and
`RAsk` surfaces the `remote_ask` callback's value via the same
result-register convention as the local `Ask` opcode (the previous
register-write mismatch returned the wrong value). Pinned by
`test_distributed_remote_address_local_fallback` and
`test_distributed_callbacks_invoked` (which now asserts the RAsk
result value). **Cross-node routing of `spawn@node` references is now
implemented (2026-08-13):** the node id is still not carried in the
actor-ref VALUE itself, but the runtime keeps a bare id → node reverse
index (`Runtime.remote_refs`, populated at remote spawn, on
SpawnResponse, on wire sends, and on inbound messages for reply-by-ref)
so `send`/`ask` on any actor-ref value routes over the wire to the
right node. A spawn@node placeholder queues messages while the
SpawnResponse is in flight and translates to the real actor id
afterwards. Pinned by `test_remote_ref_send_by_bare_id_routes_wire`,
`test_remote_ref_pending_spawn_queue_flushes`, and
`test_remote_ref_local_collision_prefers_local`. Known limits: an id
collision with a local actor prefers local delivery (the remote ref is
then unreachable by bare value — use an explicit
`ActorAddress::remote`), and the reverse index is best-effort bounded
(10k, no TTL) on top of the TTL-bounded forward cache.

Actors move between nodes explicitly with `migrate` (the `to` here is contextual syntax, not a keyword):

```nulang
// fragment
migrate cart to target_node
```

This is also accepted syntax that silently no-ops single-node: the
runtime's `migrate` handler is an empty stub, so the actor stays put and
the program continues normally with no error or observable effect.

`monitor`, `link`, and `exit` are fully implemented in-language today —
not merely "reserved for a planned surface" as this section previously
said. `spawn link Foo {}` / `spawn monitor Foo {}` and `perform
Actor.link/monitor/exit` have real runtime semantics (link propagation,
`DOWN` messages, trapped vs. non-trapping exit) and are conformance-
tested end-to-end (`actor_14` through `actor_17` in
`conformance/behavior/`). What's accurate about the old wording: they
ARE runtime-level operations (dispatched through `Runtime`, not
compiled to dedicated bytecode per-operation) — that part just doesn't
mean "planned."

## 12.5 CRDT Replication

**Implementation status (verified 2026-08-15):** the delta-sync protocol is
real (as before), `.nula`-level `state crdt` fields carry a concrete-type
selector and merge through `CrdtManager` on sync, and the `Crdt.*` effect
module + per-type operation-set enforcement + `.nula` conformance cases are
now landed (see §9.10). The example below is conforming:

```nulang
persistent actor GlobalCounter {
  state crdt gcounter count: Int = 0

  behavior increment() {
    perform Crdt.increment("count")
  }

  behavior get() {
    perform Crdt.read("count")
  }
}
```

Mutation flows through `Crdt.*` (which validates each op against the field's
type and materializes the value back into `self.count`); a raw
`self.count = expr` assignment on a crdt field is ignored. The prior
`self.count = self.count + 1` example no longer conforms — that form is
rejected as an out-of-set mutation. On a recovered actor, `self.count` still
reads the materialized value but `perform Crdt.*` is a silent nil no-op
(`field_map` is not rebuilt on recovery — see §9.10's recovery limitation).

## 12.6 Fault Tolerance

**Implementation status (verified 2026-08-15):** node failure triggers
`handle_node_failed` (`distribution.rs`): dead-node `RemoteActorCache`
invalidation and `DOWN`-with-`noconnection` to local watchers (D7 a+b).
Actor migration is real (`DistributedVmCallbacks::migrate`) with
`migrated_actors` forwarding for in-flight messages. Durable-actor
re-spawn on node loss (D7c, RFC 0014) is now implemented: a node confirmed
gone — promoted from `Failed` past `removal_confirmation_timeout` under
quorum, or immediately on a positive `Packet::NodeGoodbye` — has its
`RespawnOnNodeLoss`-opted durable actors re-spawned on their deterministic
shadow node from the last replicated snapshot (new `Packet::ShadowReplicate`
at `checkpoint_actor`, restored through `receive_migrated_actor`). A
gossip-replicated durable-actor directory (`DurableDirectoryEntry`,
highest-epoch-wins) + epoch self-demote on re-join guarantee no two live
copies. Cross-node link/monitor supervision is real (RFC 0012).

- Actor migration on node failure
- Durable-actor re-spawn on confirmed node loss (RFC 0014, opt-in per supervision edge)
- Message buffering for failed-node actors beyond the forwarding window (still absent)
- CRDT healing on node rejoin (Rust-embedder `CrdtManager` API only — see §12.5's implementation-status note; not yet reachable from `state crdt` fields)
- Supervision-based recovery (§8.8)

In-language failover constructs (`with_failover` / `on_failure`) are planned; today recovery is driven by the supervision and migration mechanisms above.

## 12.7 Network Transport

- Length-prefixed binary protocol over TCP with a node-id handshake
- Compact binary serialization of actor messages, heartbeats, acknowledgements, spawn requests/responses, and CRDT sync packets
- Actor references sent as globally unique IDs

---

# Chapter 13: WebAssembly Integration — Planned

> **Status: partially implemented.** A WASM backend exists behind the `wasm-backend` Cargo feature flag: `src/mir_wasm.rs` compiles MIR to `.wasm` modules (i64-tagged values, SIMD lowering), and `src/wasm_runtime.rs` provides a Wasmtime host runtime with AOT compilation support. The CLI exposes `--backend wasm|wasm-run|wasm-aot` when the feature is enabled. The WASM-specific surface constructs in this chapter (`@export`, `@import`, `config wasi`, `wasm { ... }` blocks) remain unimplemented; only the MIR→WASM compiler and Wasmtime runtime exist today.

## 13.1 Compilation Target

Nulang compiles to WebAssembly (Wasm), a portable, sandboxed bytecode format supported by all modern browsers and server-side runtimes. The compilation pipeline consists of five phases:

1. **Parsing**: Source code is parsed into an abstract syntax tree (AST).
2. **Type checking**: Hindley-Milner inference, reference capability analysis, and effect row verification.
3. **Effect elaboration**: Effect handler positions are determined and effect dispatch code is generated.
4. **Code generation**: The AST is lowered to WebAssembly instructions with the Nulang runtime embedded as a Wasm component.
5. **Linking**: External functions are linked through WIT (Wasm Interface Types) interfaces.

The WebAssembly target provides several advantages for Nulang: near-native execution speed, sandboxed security through capability-based WASI integration, cross-platform deployment without recompilation, and seamless composition with other Wasm modules written in any language.

The Nulang runtime is embedded within each compiled Wasm module. This runtime includes the actor scheduler, garbage collector, effect dispatch mechanism, and (for persistent actors) the durability layer. When a Nulang program starts, the `__nulang_start` export initializes the runtime before executing module-level code.

## 13.2 WIT Interface Generation

Nulang can both generate and consume WIT (Wasm Interface Types) interfaces for inter-language composition. The `@export` annotation exposes a Nulang function to other Wasm modules, while the `@import` annotation imports functions from other modules.

```nulang
-- Nulang function exposed to other Wasm modules
@export
let compute_hash: (String) -> String = (input) -> {
  perform crypto.sha256(input)
}

-- Import a function from another Wasm module
@import(module = "env", name = "external_api")
let external_api: (Int) -> String

-- Import an entire interface
@import_interface(module = "logging")
let logger: LoggerInterface
```

The compiler automatically generates WIT files from `@export` annotations and validates `@import` annotations against available WIT interfaces. Type mappings between Nulang types and WIT types are handled automatically: records map to WIT records, variants map to WIT variants, strings map to WIT strings, and arrays map to WIT lists.

## 13.3 WASI Worlds

Nulang programs run within WASI (WebAssembly System Interface) worlds that define the sandboxed system resources available to the program. The WASI world determines which system calls the program may perform, and Nulang's authority capabilities map directly to WASI capability grants.

```nulang
-- Configure the WASI world for this program
config wasi {
  world = "command"
  allowed_dirs = ["/tmp", "/data"]
  allowed_env = ["API_KEY", "DATABASE_URL"]
  allowed_network = ["api.example.com:443"]
}
```

| Nulang Capability | WASI Rights |
|-------------------|-------------|
| `file` | `path_open`, `fd_read`, `fd_write`, `fd_seek` |
| `network` | `sock_open`, `sock_connect`, `sock_send`, `sock_recv` |
| `random` | `random_get` |
| `time` | `clock_time_get`, `clock_time_set` |

When a Nulang program attempts to perform an effect that the WASI world does not permit, the effect handler raises an appropriate error rather than causing a runtime trap. This provides graceful degradation: a program that cannot access the filesystem receives a `FileSystem` error that can be handled by the programmer, rather than an opaque sandbox violation.

## 13.4 Cross-Language Composition

Nulang actors can spawn and communicate with actors implemented in other languages that compile to WebAssembly. This enables polyglot systems where each component is written in the most appropriate language while maintaining the uniform actor interface.

```nulang
-- Spawn an actor implemented in Rust
let rust_service = spawn @wasm("rust_service.wasm") DataProcessor

-- Send messages as if it were a Nulang actor
rust_service <- process(data)

-- Receive responses normally
let result = ask(rust_service, query(q))
```

The runtime marshals messages between Nulang's internal representation and the WIT interface types used by the foreign module. Actor references, promises, and exceptions are all translated correctly across language boundaries.

Nulang modules can also be used as libraries from other languages. A Nulang module compiled with `@export` annotations on its public functions can be imported and called from Rust, Go, Python, or JavaScript via standard Wasm component bindings.

## 13.5 Building Blocks

The Nulang runtime is organized into composable Wasm components that can be selectively included in the compiled output:

| Component | Description | Optional? |
|-----------|-------------|-----------|
| **Actor runtime** | Core scheduler, message passing, actor lifecycle | No |
| **GC** | Reference-counting + cycle-collecting garbage collector | No |
| **Effect handlers** | Default handlers for IO, FileSystem, Network, Time, Random | Yes (custom handlers can replace) |
| **Persistence layer** | Snapshotting, event journaling, recovery | Yes (only for `persistent` actors) |
| **AI connector** | LLM provider integration, embedding client | Yes (only for actors with `llm` capability) |
| **Cluster transport** | Inter-node message routing, gossip, CRDT sync | Yes (only for distributed deployments) |

Each component adds to the final Wasm module size. A simple single-threaded program with no actors compiles to a module of approximately 50KB. A full distributed persistent AI-augmented service compiles to approximately 2MB.

---

# Chapter 14: Standard Library — Planned

> **Status: not implemented.** There is currently no standard library and no prelude: no module is automatically imported, and common types such as `Option`/`Result`/`List`/`Map` must be declared by the program itself (as variants/records) or represented with built-in arrays. The only built-in modules the compiler recognizes are the AI-orchestration modules `Pipeline`, `Supervisor`, and `Debate` (§11.6). This chapter describes the planned standard library and is retained as the design reference; all modules and functions in it are unimplemented.

## 14.1 Core Module

The `Core` module provides the fundamental types, functions, and typeclasses that all Nulang programs depend on. It is automatically imported in every module and does not require an explicit `import` statement.

Key components of the Core module:

- **Identity and constant functions**: `identity`, `constant`, `flip`, `compose`
- **Comparison utilities**: `compare`, `min`, `max`, `clamp`
- **Result and Option utilities**: `Result.map`, `Result.flat_map`, `Option.get_or`, `Option.map`
- **Typeclasses**: `Eq`, `Ordered`, `Numeric`, `Show`, `Semigroup`, `Monoid`, `Functor`, `Applicative`, `Monad`

```nulang
-- Core functions are always available
let id = identity(42)                       -- 42
let c = constant(7)("hello")               -- 7
let cmp = compare(3, 5)                     -- LessThan
let bounded = clamp(0, 100, 150)            -- 100

-- Result/Option utilities
let mapped = Result.map(Ok(42), (x) -> x * 2)   -- Ok(84)
let fallback = Option.get_or(None, "default")    -- "default"
```

## 14.2 IO Module

The `IO` module provides the default handler for the `IO` effect, implementing console input and output operations.

```nulang
import IO

perform io.println("Hello, World!")
let name = perform io.read_line()
perform io.print("You entered: ")
perform io.println(name)

-- Formatted output
perform io.println("Value: {value}, Count: {count}")

-- File descriptors (via WASI)
perform io.write_to(1, "stdout message\n")   -- fd 1 = stdout
perform io.write_to(2, "error message\n")    -- fd 2 = stderr
```

The `IO` module also provides convenience functions for common patterns:

```nulang
perform io.print_lines(["Line 1", "Line 2", "Line 3"])
let lines = perform io.read_all_lines()
```

## 14.3 Collections

The standard library provides several persistent collection types with rich APIs:

### List

`List[T]` is a singly-linked list optimized for prepend and sequential access.

```nulang
import List

let numbers = [1, 2, 3, 4, 5]
let doubled = List.map(numbers, (x) -> x * 2)           -- [2, 4, 6, 8, 10]
let evens = List.filter(numbers, (x) -> x % 2 == 0)     -- [2, 4]
let total = List.fold(numbers, 0, (a, b) -> a + b)      -- 15
let has_three = List.contains(numbers, 3)               -- true
let length = List.length(numbers)                        -- 5
let reversed = List.reverse(numbers)                     -- [5, 4, 3, 2, 1]
let sorted = List.sort([3, 1, 4, 1, 5])                  -- [1, 1, 3, 4, 5]
let zipped = List.zip([1, 2, 3], ["a", "b", "c"])       -- [(1, "a"), (2, "b"), (3, "c")]
```

### Map

`Map[K, V]` is an immutable hash map with O(1) average-case lookup.

```nulang
import Map

let phonebook = Map.from_list([("Alice", "555-1234"), ("Bob", "555-5678")])
let alice_phone = Map.get(phonebook, "Alice")                  -- Some("555-1234")
let updated = Map.insert(phonebook, "Carol", "555-9012")
let without_bob = Map.delete(phonebook, "Bob")
let keys = Map.keys(phonebook)                                   -- ["Alice", "Bob"]
let merged = Map.merge(phonebook, additional_entries)
```

### Set

`Set[T]` is an immutable hash set.

```nulang
import Set

let tags = Set.from_list(["nulang", "programming", "actors"])
let has_tag = Set.contains(tags, "actors")                     -- true
let combined = Set.union(tags, Set.from_list(["wasm", "ai"]))
let common = Set.intersection(tags, Set.from_list(["actors", "rust"]))
let added = Set.insert(tags, "distributed")
```

## 14.4 String Processing

The `String` module provides comprehensive string manipulation functions.

```nulang
import String

let trimmed = String.trim("  hello  ")                          -- "hello"
let parts = String.split("a,b,c", ",")                         -- ["a", "b", "c"]
let replaced = String.replace("hello world", "world", "Nulang") -- "hello Nulang"
let prefixed = String.starts_with("nulang", "nu")              -- true
let suffixed = String.ends_with("nulang", "lang")              -- true
let lower = String.to_lowercase("HELLO")                       -- "hello"
let upper = String.to_uppercase("hello")                       -- "HELLO"
let substring = String.slice("nulang", 0, 2)                   -- "nu"
let joined = String.join(["a", "b", "c"], ",")                -- "a,b,c"
let parsed_int = String.parse_int("42")                        -- Some(42)
let parsed_float = String.parse_float("3.14")                  -- Some(3.14)
```

## 14.5 Concurrency Primitives

The `Concurrent` module provides actor-level concurrency utilities beyond basic message passing.

```nulang
import Concurrent

-- Spawn an actor
let worker = spawn Worker

-- Fire-and-forget message sending
worker <- do_work(data)

-- Request-response pattern
let promise = ask(worker, compute(data))
let result = await promise

-- Timeout on await
let result = await promise within Duration.seconds(5)

-- Select from multiple promises
let first = select {
  | promise_a => handle_a
  | promise_b => handle_b
  | timeout(Duration.seconds(10)) => handle_timeout
}

-- Parallel map using actors
let results = parallel_map(items, (item) -> {
  let worker = spawn Processor
  ask(worker, process(item))
})
```

## 14.6 HTTP Client/Server

**Implementation status: real, but with different syntax and a real
coexistence limitation.** `Http.get(url) -> String` / `Http.post(url,
body) -> String` (client) and `Http.serve(port, handler) -> Int` (a real
OS listener; returns the bound port, or a positive port even for `0`
meaning "pick one") are genuine, wired effects (`src/vm.rs`,
`src/stdlib.rs`) — but as bare `perform Http.*` calls, not through an
`import Http` module, and not with the request/response record types,
headers, or `http.*` lowercase spelling shown below and elsewhere in
this section (those don't exist — no request object, no response
record, client calls return a bare `String` body or `nil` on failure).
`Http.serve` is dispatched only when a program has an actor/workflow
declaration; `Http.get`/`Http.post` are dispatched only when it doesn't
(`src/main.rs`'s runtime-vs-standalone split) — the two currently can't
be used in the same file (`examples/17_actor_fetcher.nula` already
documents this as a known VM limitation).

The `Http` module provides both client and server functionality through the `Network` effect.


### HTTP Client

```nulang
import Http

-- Simple GET request
let response = perform http.get("https://api.example.com/users")

-- Request with headers
let response = perform http.request(
  method = Get,
  url = "https://api.example.com/users",
  headers = [("Authorization", "Bearer " ++ token)],
  body = None
)

-- POST with JSON body
let response = perform http.post(
  url = "https://api.example.com/users",
  headers = [("Content-Type", "application/json")],
  body = Some(Json.stringify(new_user))
)

-- Response handling
match response {
  | Ok(resp) => {
      perform io.println("Status: " ++ Int.to_string(resp.status))
      perform io.println("Body: " ++ resp.body)
    }
  | Error(e) => perform io.println("Request failed: " ++ e)
}
```

### HTTP Server

```nulang
actor HttpServer {
  capability http
  capability network

  behavior start(port: Int) {
    perform http.serve(port, (request) -> {
      match (request.method, request.path) {
        | (Get, "/health") =>
            Http.Response { status = 200, body = "OK", headers = [] }
        | (Get, "/users") =>
            let users = perform database.query("SELECT * FROM users")
            Http.Response {
              status = 200,
              body = Json.stringify(users),
              headers = [("Content-Type", "application/json")]
            }
        | _ =>
            Http.Response { status = 404, body = "Not Found", headers = [] }
      }
    })
  }
}
```

## 14.7 Serialization

**Implementation status: JSON is real with a different, simpler API;
Binary doesn't exist.** `import stdlib::json` (not `import Json`) gives
unqualified `parse(json: String) -> JsonValue` and
`stringify(value: JsonValue) -> String`, where `JsonValue` is a closed
variant (`JsonNull | JsonBool | JsonNumber(Float) | JsonString |
JsonArray | JsonObject`) — not the generic record/`Result`-returning API
shown below. `parse` never fails (lenient by design: malformed input
degrades gracefully rather than erroring) and there is no
`stringify_pretty`, `get_field`/`get_field_as`, or schema-driven
deserialization; field access is `get_string`/`get_number`/`get_bool`
with a required default value. `Binary` (serialize/deserialize/
deserialize_with_schema) has no implementation at all — `import Binary`
fails to resolve.

The `Json` and `Binary` modules provide data serialization and deserialization.


### JSON

```nulang
import Json

-- Serialization
let user = { name = "Alice", age = 30, active = true }
let json = Json.stringify(user)
-- Result: "{\"name\":\"Alice\",\"age\":30,\"active\":true}"

-- Pretty printing
let pretty = Json.stringify_pretty(user)

-- Deserialization with type inference
let parsed: Result[{ name: String, age: Int, active: Bool }, String] =
  Json.parse(json)

-- Safe field access
let result = Json.parse(json)
match result {
  | Ok(user) => perform io.println("Hello, " ++ user.name)
  | Error(e) => perform io.println("Parse error: " ++ e)
}

-- Partial parsing
let name = Json.get_field(json, "name")           -- Some(Json.String("Alice"))
let age = Json.get_field_as(json, "age", Int)     -- Some(30)
```

### Binary Serialization

```nulang
import Binary

-- Serialize to binary format
let bytes = Binary.serialize(user)

-- Deserialize from binary
let restored: Result[User, String] = Binary.deserialize(bytes)

-- Schema evolution support
let compatible = Binary.deserialize_with_schema(bytes, UserSchema.v2)
```

---

## 14.5 Determinism Contract

Nulang provides a strict determinism contract for sequential Core computation,
ensuring a program run in 2026 produces bit-identical output in 2226 across
all conforming runtimes:

1. **Floating-point:** all `Float` arithmetic is IEEE-754 binary64 using
   round-to-nearest, ties-to-even. Fast-math optimizations that break this
   are prohibited. NaN payloads are not preserved (they are canonicalized by
   the value layout).
2. **Scheduling:** when multiple actors have the same priority and are ready
   to run, the scheduler dequeue order is strictly FIFO based on enqueue time.
3. **GC:** garbage collection observability (e.g. weak references, if ever
   added) is strictly ordered by logical time, not wall-clock time.

A planned Stable-tier extension is a full Numeric Tower (`Int`, `BigInt`,
`Rat`) to guarantee 200-year overflow-free arithmetic, as `i64` alone is
insufficient for durable long-horizon computation.


# Chapter 15: Operational Model — Planned

> **Status: mostly not implemented.** The deployment manifest, configuration system, observability exporters, and operational tooling described in this chapter are planned. What exists today is the `nulang` command-line tool:
>
> - `nulang file.nula` — compile and run a source file
> - `nulang --eval 'expr'` / `-e` — evaluate a source string
> - `nulang --check file.nula` / `-c` — type, effect, and capability checking only (no execution)
> - `nulang --repl` / `-r` — interactive read-eval-print loop
> - `nulang --verbose` / `-v` — print AST, bytecode, and inferred types while running
> - `nulang --lsp` — start the stdio Language Server (tower-lsp)
> - `nulang --backend bytecode|native` — select execution backend (default: bytecode)
> - `nulang --emit-nbc --out <file>` — compile to a frozen `.nbc` bytecode artifact
> - `nulang file.nbc` — run a pre-compiled `.nbc` artifact directly
> - `nulang --verify <src> file.nbc` — verify `.nbc` source hash against `<src>`
> - `nulang --doc` — generate Markdown API docs (`docs/api.md`)
> - `nulang --emit-stdlib-docs <dir>` — generate per-effect stdlib docs
> - `nulang --color auto|always|never` — colorize error output
> - `nulang --version` / `-V`, `nulang --help` / `-h`
> - `nulang nula new|build|test|run` — package manager commands
>
> When the `wasm-backend` Cargo feature is enabled, `--backend` also accepts `wasm|wasm-run|wasm-aot`.

## 15.1 Deployment

Nulang applications are deployed as WebAssembly modules with an embedded manifest that describes the application's requirements and configuration.

The deployment manifest (`nulang.toml`) specifies:

```nulang
-- nulang.toml: deployment manifest
[package]
name = "my-service"
version = "1.0.0"
description = "Example Nulang service"

[deployment]
type = "actor-service"
min_instances = 2
max_instances = 10
auto_scale = true

[capabilities]
llm = true
http = true
database = true
filesystem = false
network = true

[persistence]
enabled = true
storage_backend = "s3"       -- or "local", "postgres"
snapshot_interval = 1000
journal_retention = "30d"

[cluster]
enabled = true
replication_factor = 3

[ai]
provider = "openai"
model = "gpt-4"
fallback_model = "gpt-3.5-turbo"
```

Deployment targets include:

| Target | Description |
|--------|-------------|
| **Edge runtime** | Single-node execution on edge devices |
| **Server runtime** | Multi-process execution on servers |
| **Cluster runtime** | Distributed execution across a cluster |
| **Serverless** | Function-as-a-service with cold-start optimization |
| **Browser** | Client-side execution via WebAssembly |

## 15.2 Configuration

Configuration is provided through a hierarchy of sources, with later sources overriding earlier ones:

1. Default values from the configuration schema
2. `nulang.toml` deployment manifest
3. Environment variables (prefixed with `NULANG_`)
4. Command-line flags
5. Runtime API calls

```nulang
-- config.nula
config app {
  port = env("PORT", 8080)
  database_url = env("DATABASE_URL")
  log_level = env("LOG_LEVEL", "info")
  max_connections = env("MAX_CONNECTIONS", 100)
}

-- Accessing configuration values
let port = config.app.port
let db_url = config.app.database_url
```

Configuration values are typed and validated at startup. Invalid values produce clear error messages indicating the expected type and the actual value received.

## 15.3 Observability

Nulang provides built-in observability through three pillars: logging, metrics, and tracing.

> **Implementation status (verified 2026-08-14):** the distributed
> observability slice landed: `otel` feature + `init_tracing`
> (`src/observability.rs`), `--metrics-port` Prometheus server
> (`src/main.rs:804/1727`), and a `trace_id` carried cross-node in
> `Packet::ActorMessage` (`network.rs:547-549`). The `.nula` effects below
> (`metrics.counter/histogram/gauge/timer`, `trace.span/annotate`, `config
> trace`) are **planned surface** (backlog).

### Structured Logging

```nulang
-- Log levels: debug, info, warn, error, fatal
perform io.log_debug("Processing item", { item_id = item.id, index = i })
perform io.log_info("Order completed", { order_id = order.id, total = order.total })
perform io.log_warn("High latency detected", { endpoint = "/api/users", latency_ms = 500 })
perform io.log_error("Payment failed", { order_id = order.id, reason = error_msg })
```

Logs are emitted in structured JSON format by default, with configurable output formats (JSON, pretty, syslog).

### Metrics

```nulang
-- Counter (monotonically increasing)
perform metrics.counter("orders.processed", 1)
perform metrics.counter("requests.total", 1, { method = "GET", path = "/users" })

-- Histogram (distribution of values)
perform metrics.histogram("request.duration_ms", elapsed)
perform metrics.histogram("response.size_bytes", body_length)

-- Gauge (point-in-time value)
perform metrics.gauge("active_connections", current_connections)

-- Timer (convenience for timing blocks)
let result = perform metrics.timer("database.query", {
  perform database.query(sql)
})
```

Metrics are exported via OpenTelemetry by default, with adapters for Prometheus, StatsD, and cloud monitoring services.

### Tracing

```nulang
-- Manual span creation
perform trace.span("process_order", {
  perform trace.annotate("order_id", order.id)
  let validated = perform trace.span("validate", { validate(order) })
  let charged = perform trace.span("charge", { charge_payment(order) })
  let shipped = perform trace.span("ship", { ship_order(order) })
  { validated = validated, charged = charged, shipped = shipped }
})

-- Automatic actor message tracing
config trace {
  auto_trace_actor_messages = true
  sample_rate = 0.1    -- Trace 10% of messages
}
```

## 15.4 Debugging

Nulang provides several debugging capabilities for development and production environments.

> **Implementation status (verified 2026-08-14):** `perform
> debug.inspect(actor_id)` is landed — the runtime returns
> `{ state, mailbox_size, behaviors, supervisor }` (`callbacks.rs:438-455`);
> a standalone `Debug.inspect(label, value)` print form exists
> (`vm.rs:664`). `debug.trace_messages` and `debug.snapshot` are
> **planned surface** (backlog).

### Actor Inspection

```nulang
-- Query actor state and mailbox at runtime
perform debug.inspect(actor_id)
-- Returns: { state, mailbox_size, behaviors, supervisor }

-- Trace message flow
perform debug.trace_messages(actor_id, duration = Duration.minutes(1))

-- Snapshot actor state for offline analysis
let snapshot = perform debug.snapshot(actor_id)
```

### Deterministic Replay

Persistent actors support deterministic replay for debugging:

```nulang
-- Replay a persistent actor from the beginning
debug replay OrderProcessor {
  from = "start"
  breakpoints = ["create_order"]
}

-- Replay from a specific snapshot
debug replay OrderProcessor {
  from = Snapshot.at(10_000)
  speed = Realtime
}
```

### Interactive Shell

The Nulang interactive shell connects to a running system for exploration:

```nulang
$ nulang shell --target production-cluster
Connected to cluster (12 nodes, 4,832 actors)

> list actors --type ShoppingCart
actor://ShoppingCart/user-123   (node-3, 12 messages/sec)
actor://ShoppingCart/user-456   (node-7, 3 messages/sec)
actor://ShoppingCart/user-789   (node-1, 0 messages/sec)

> inspect actor://ShoppingCart/user-123
State: { items = [CartItem {...}, CartItem {...}], last_updated = 2025-01-15T10:30:00Z }
Mailbox: 2 pending messages
Behaviors: add_to_cart, remove_from_cart, get_cart, checkout

> send actor://ShoppingCart/user-123 get_cart()
Response: [CartItem { product = "Book", qty = 2 }, CartItem { product = "Pen", qty = 5 }]
```

## 15.5 Performance Tuning

Performance tuning options for Nulang applications:

### Scheduler Configuration

```nulang
config runtime.scheduler {
  thread_count = 8                    -- Number of scheduler threads
  work_stealing = true                -- Enable work stealing between threads
  affinity = true                     -- Pin threads to CPU cores
  spin_before_park = 100              -- Spin iterations before parking
}
```

### GC Tuning

```nulang
config runtime.gc {
  collection_interval = 1000          -- GC cycle interval in ms
  heap_size_hint = "1gb"             -- Target heap size
  nursery_size = "256mb"             -- Young generation size
  full_gc_threshold = 0.75           -- Heap usage threshold for full GC
}
```

### Network Tuning

```nulang
config runtime.network {
  tcp_nodelay = true                  -- Disable Nagle's algorithm
  buffer_size = 65536                 -- Network buffer size
  max_message_size = "16mb"          -- Maximum message size
  connection_pool_size = 100          -- Connection pool per node
}
```

## 15.6 Security

### Capability Audit

```nulang
-- List all actors and their capabilities
let audit = perform security.audit_capabilities()

-- Verify no actor has unexpected capabilities
let violations = List.filter(audit, (a) -> not is_authorized(a))
```

### Network Security

- All inter-node communication is encrypted via TLS
- Certificate pinning for cluster nodes
- Mutual TLS authentication

### Sandboxing

- WASI capabilities restrict system access
- Effect handlers can implement additional sandboxing
- Resource limits (CPU, memory, file descriptors) enforced by runtime

---

# Appendix A: Grammar Reference

## A.1 Grammar Notation

This appendix uses Extended Backus-Naur Form (EBNF) notation:

- `|` separates alternatives
- `"..."` surrounds literal strings
- `( ... )` groups elements
- `[ ... ]` denotes optional elements
- `{ ... }` denotes zero or more repetitions
- `{ ... }-` denotes one or more repetitions

## A.2 Top-Level Grammar

This grammar reflects the current parser (as of July 2026). Forms marked † are lexed/reserved but not yet wired into the parser.

```
module        ::= { declaration } [ top_level_expression ]

declaration   ::= [ annotations ] [ "pub" ] decl_head

annotations   ::= { "@tool" "(" "description" ":" string ")" }

decl_head     ::= function_definition
                | agent_definition
                | workflow_definition
                | type_definition
                | type_alias
                | actor_definition
                | effect_definition
                | extern_block
                | import_declaration
                | module_definition

function_definition ::= "fn" identifier [ type_params ] "(" [ parameters ] ")"
                        [ "->" type ] [ "!" effect_row ] [ ":" capability ] expression

type_params   ::= "[" identifier { "," identifier } "]"

parameters    ::= identifier [ ":" type ] { "," identifier [ ":" type ] }

type_definition ::= "type" identifier [ type_params ] "="
                    ( record_type | variant_type )

variant_type  ::= [ "|" ] constructor { "|" constructor }

constructor   ::= identifier [ "(" type ")" ]

type_alias    ::= "type" "alias" identifier [ type_params ] "=" type

module_definition ::= "module" identifier "{" { declaration } "}"

import_declaration ::= "import" identifier   (dotted module path)

extern_block  ::= "extern" string "{" { "fn" identifier "(" [ parameters ] ")" [ "->" type ] } "}"
```

## A.3 Expression Grammar

```
expression    ::= literal
                | identifier
                | prefix_op expression
                | expression infix_op expression
                | application
                | field_access
                | index_expr
                | send_infix
                | lambda
                | let_binding
                | let_rec
                | conditional
                | match_expr
                | handle_expr
                | perform_expr
                | actor_expr
                | loop_expr
                | block

application   ::= expression "(" [ arguments ] ")"

field_access  ::= expression "." ( identifier | integer )   (incl. self.field, tuple .0)

index_expr    ::= expression "[" expression "]"

send_infix    ::= expression "!" identifier "(" [ arguments ] ")"

arguments     ::= expression { "," expression }

prefix_op     ::= "-" | "not" | "!" | "&" | "*"

lambda        ::= "fn" "(" [ parameters ] ")" [ "->" ] expression

let_binding   ::= "let" identifier [ ":" type ] "=" expression "in" expression

let_rec       ::= "let" "rec" identifier "(" [ parameters ] ")" "=" expression "in" expression

conditional   ::= "if" expression [ "then" ] ( expression | block ) [ "else" ( expression | block ) ]

match_expr    ::= "match" expression [ "with" ] "{" { [ "case" | "|" ] pattern [ "if" expression ] "=>" expression } "}"

handle_expr   ::= "handle" expression "{" { handler_clause } "}"

handler_clause ::= "|" identifier "." identifier "(" [ identifiers ] ")" [ "resume" ] "=>" expression

perform_expr  ::= "perform" identifier "." identifier "(" [ arguments ] ")"

actor_expr    ::= "spawn" expression "{" { identifier "=" expression } "}"
                | "send" expression identifier "(" [ arguments ] ")"
                | "ask" expression identifier "(" [ arguments ] ")"
                | "receive" "{" { "|" identifier "(" [ identifiers ] ")" "=>" expression } "}"
                | "migrate" expression "to" expression
                | "emit" identifier "(" [ arguments ] ")"
                | "self"

loop_expr     ::= "for" identifier "in" expression expression
                | "break" | "return" [ expression ]

block         ::= "{" { expression ( ";" | newline ) } "}"
```

Reserved but unimplemented: `recover` (capability recovery), `var` (mutable bindings), `loop` (unconditional loops), `monitor` / `link` / `exit` (process lifecycle), `subworkflow` / `await` (workflow composition), `stop` (actor shutdown), `capability` (authority capabilities).

## A.4 Pattern Grammar

```
pattern       ::= identifier
                | "_"
                | literal
                | constructor_pattern
                | record_pattern
                | tuple_pattern
                | alias_pattern

constructor_pattern ::= identifier [ "(" pattern ")" ]   (single payload)

record_pattern ::= "{" { identifier ":" pattern } "}"

tuple_pattern  ::= "(" [ patterns ] ")"

alias_pattern ::= identifier "@" pattern

patterns      ::= pattern { "," pattern }
```

(Pattern guards are supported: an arm may place `if expression` between the pattern and `=>`; the guard must have type `Bool` and may reference pattern-bound variables.)

## A.5 Type Grammar

```
type          ::= primitive_type
                | type_constructor
                | function_type
                | tuple_type
                | record_type
                | array_type
                | capability_type

primitive_type ::= "Int" | "Float" | "Bool" | "String" | "Unit" | "Nil" | "Never" | "Address"

type_constructor ::= identifier [ "[" types "]" ]

function_type ::= "fn" "(" [ types ] ")" "->" type [ "!" effect_row ] [ ":" capability ]

tuple_type    ::= "(" [ types ] ")"

record_type   ::= "{" { identifier ":" type } "}"

array_type    ::= "[" type "]"

capability_type ::= "&" capability type

capability    ::= "lineariso" | "iso" | "trn" | "ref" | "val" | "box" | "tag"

effect_row    ::= "{" [ effect_refs ] [ "|" row_var ] "}" | identifier

effect_refs   ::= identifier { "," identifier }

types         ::= type { "," type }
```

## A.6 Actor Grammar

```
actor_definition ::= [ "persistent" ] "actor" identifier [ type_params ] "{" { actor_member } "}"

actor_member  ::= state_declaration
                | behavior_declaration

state_declaration ::= "state" [ state_model ] identifier [ ":" type ] "=" expression

state_model   ::= "local" | "durable" | "event_sourced" | "crdt"   (default: local)

behavior_declaration ::= "behavior" identifier "(" [ parameters ] ")"
                         [ "!" effect_row ] [ ":" capability ] expression
```

## A.7 Workflow Grammar

```
workflow_definition ::= "workflow" identifier "{" { workflow_member } "}"

workflow_member ::= step_declaration
                  | parallel_block
                  | compensate_block

step_declaration ::= "step" identifier "{" expression "}" [ "compensate" "{" expression "}" ]

parallel_block ::= "parallel" "{" { step_declaration } "}"

compensate_block ::= "compensate" "{" expression "}"
```

## A.8 Agent Grammar

```
agent_definition ::= "agent" identifier "=" "{" { agent_field } "}"

agent_field   ::= "model" ":" string
                | "system_prompt" ":" string
                | "tools" ":" "[" { identifier } "]"
                | "memory" ":" "{" [ "max_turns" ":" integer ] "}"
                | "semantic_memory" ":" "{" [ "dimensions" ":" integer ] "}"
                | "procedural_memory" ":" "{" [ "namespace" ":" string ] "}"
                | "pricing" ":" "{" [ "input" ":" number ] [ "output" ":" number ] "}"
```

---

# Appendix B: Built-in Types Reference

## B.1 Primitive Types

| Type | Size | Range/Description |
|------|------|-------------------|
| `Bool` | 1 byte | `true` or `false` |
| `Int` | 64 bits | Signed 64-bit integer (i64) |
| `Float` | 64 bits | IEEE 754 double-precision (f64) |
| `String` | — | UTF-8 string |
| `Unit` | 0 bytes | The unit value `()` (also `unit`) |
| `Nil` | — | The `nil` value; absence of a value |
| `Never` | — | The empty type (no values) |
| `Address` | — | Opaque actor/node address |
| `Decimal` | — | *Planned* — arbitrary-precision decimal |
| `Char` | — | *Planned* — single Unicode scalar value |

## B.2 Collection Types — Planned

Only the built-in fixed-size array `[T]` exists today (indexed load/store, `arr[i]`). The following persistent collections are planned standard-library types:

| Type | Description | Complexity |
|------|-------------|------------|
| `List[T]` | Immutable linked list | Prepend: O(1), Access: O(n) |
| `Array[T]` | Immutable array | Access: O(1), Append: O(n) |
| `Map[K, V]` | Immutable hash map | Lookup: O(1), Insert: O(n) |
| `Set[T]` | Immutable hash set | Contains: O(1), Insert: O(n) |

## B.3 Actor Types

| Type | Description |
|------|-------------|
| `Address` | Opaque reference to an actor (the implemented actor-reference type) |
| `Promise[T]` | *Planned* — a future value from an async operation |
| `Mailbox[T]` | *Planned* — an actor's message queue as a first-class type |

## B.4 CRDT Types

These 8 types exist and are tested at the Rust level (`src/runtime/crdt.rs`,
`crdt_reg.rs`). The concrete-type selector (`gcounter | pncounter | gset |
orset | aworset | lwwregister | mvregister | rga`) is landable from
`.nula` `state crdt` declarations; a `Crdt.*` effect module and per-type
operation-set enforcement are not yet implemented. See §9.10 and §12.5.

| Type | Description | Merge Strategy |
|------|-------------|----------------|
| `GCounter` | Grow-only counter | Max of replica values |
| `PNCounter` | Positive-negative counter | Component-wise max |
| `GSet[T]` | Grow-only set | Union |
| `ORSet[T]` | Observed-remove set | Add-wins |
| `AWORSet[T]` | Add-wins observed-remove set | Add-wins |
| `LWWRegister[T]` | Last-write-wins register | Timestamp comparison |
| `MVRegister[T]` | Multi-value register | Concurrent values preserved |
| `RGA[T]` | Replicated growable array | Tombstone-based merge |

## B.5 Capability Types

| Capability | Read | Write | Sendable | Use Case |
|------------|------|-------|----------|----------|
| `lineariso` | Yes | Yes | Yes | Unique ownership, consumed exactly once |
| `iso` | Yes | Yes | Yes | Unique ownership |
| `trn` | Yes | Yes | No | Transitioning to val |
| `ref` | Yes | Yes | No | Local mutable reference |
| `val` | Yes | No | Yes | Immutable shared data |
| `box` | Yes | No | No | Read-only view |
| `tag` | No | No | Yes | Opaque identity |

---

# Appendix C: Effect Reference

The effect checker recognizes the built-in effect names `IO`, `Net` (alias of `Http`), `Http`, `FS`, `Array`, `String`, `Test`, `Rand` (alias of `Random`), `Random`, `Time`, `Spawn`, `Send`, `Receive`, `Migrate`, `STM`, `Async`, `Inference`, `Cost`, `Event`, `FFI`, `DB`, `Python`, `Process`, and `System` (`parse_effect_name`, `src/effect_checker.rs`; §4.6). `LLM` is accepted as a deprecated alias for `Inference` (`src/cir_lower.rs`). Two caveats:

- **Recognition is not dispatch.** A recognized name participates in effect-row checking, but runtime backing exists only for the operations listed in §4.6 (`IO.*`, `FS.*`, `Http.*`, `Random.int`, `Time.now`, `Inference.ask`/`LLM.ask`, and context-limited `Timer.sleep`/`Signal.wait`). The aliases are compile-time only: `Net` and `Rand` type-check but have **no runtime dispatch** — the VM dispatches on the literal names `Http` and `Random` (`src/vm.rs`), so `perform Net.get(...)` / `perform Rand.int(...)` fail with "Unhandled effect". Use `Http` / `Random` in performed code.
- `Spawn`, `Send`, `Receive`, and `Migrate` are language syntax (keywords / a bytecode op), not `perform`-dispatchable effects (§4.6).

These names do not come with pre-declared operation sets — programs declare the operations they use with `effect` declarations.

The declarations below are **illustrative** — they show the planned standard-library effect surface written in current syntax. They are not shipped with the implementation.

## C.1 Console Effect (illustrative)

```
effect Console {
  println: (String) -> Unit
  read_line: () -> String
  print: (String) -> Unit
}
```

## C.2 FileSystem Effect (illustrative)

```
effect FileSystem {
  read: (String) -> String
  write: (String, String) -> Unit
  exists: (String) -> Bool
  delete: (String) -> Unit
  list_dir: (String) -> List[String]
}
```

## C.3 Network Effect (illustrative)

```
effect Network {
  get: (String) -> Response
  post: (String, String) -> Response
  put: (String, String) -> Response
  delete: (String) -> Response
  request: (Request) -> Response
}
```

## C.4 Random Effect (illustrative)

```
effect Random {
  int: () -> Int
  float: () -> Float
  bool: () -> Bool
  int_range: (Int, Int) -> Int
}
```

## C.5 Time Effect (illustrative)

```
effect Time {
  now: () -> Timestamp
  sleep: (Int) -> Unit
}
```

## C.6 Inference Effect

`Inference.ask` (formerly `LLM.ask`) is runtime-backed today; the broader planned surface:

```
effect Inference {
  ask: (String) -> String
  complete: (String, InferenceOptions) -> InferenceResponse
  embed: (String) -> Embedding
  tool_call: (String, List[Tool]) -> ToolResult
}
```

> **Compatibility note:** The name `LLM` (effect name, row name, and `LLM.ask` operation) remains accepted as a deprecated alias for `Inference`. New code should use `Inference`.

## C.7 Metrics Effect (illustrative)

```
effect Metrics {
  counter: (String, Int, Map[String, String]) -> Unit
  histogram: (String, Float, Map[String, String]) -> Unit
  gauge: (String, Float, Map[String, String]) -> Unit
}
```

## C.8 Trace Effect (illustrative)

```
effect Trace {
  span: (String, fn() -> T) -> T
  annotate: (String, String) -> Unit
}
```

---

# Appendix D: Migration Guide from v1 to v2 — Planned

> **Status: illustrative.** This guide describes a planned v1→v2 migration path and its examples predate the current syntax. Two corrections against the current implementation: the `agent` keyword is **not** removed — it is a live v0.9 feature (§11.2); and the "After" examples below use old surface syntax (`capability llm`, bare state access, `perform llm.complete`) — the current forms are effect rows (`! {LLM}`), `self.field` state access, and `perform LLM.ask(...)`. No `nulang migrate` tool exists today (§D.8 is planned).

## D.1 Overview

This guide helps developers migrate Nulang 1.x code to Nulang 2.0. The key changes are:

1. **Actors replace agents.** The `agent` keyword is removed. Use `actor` with `capability llm` instead.
2. **Effects replace special syntax.** `perform llm.complete()` replaces `agent.llm.complete()`.
3. **Persistent actors replace separate storage.** Use `persistent actor` with `state durable` instead of external storage.
4. **Workflows are now actors.** The `workflow` keyword creates a special kind of persistent actor.
5. **State models are explicit.** Declare `local`, `durable`, `event_sourced`, or `crdt` for each state variable.

## D.2 Agent to Actor Migration

### Before (v1.x)

```nulang
agent ChatBot {
  memory short_term
  
  on message {
    let response = llm.complete(user_message)
    reply(response)
  }
}
```

### After (v2.0)

```nulang
actor ChatBot {
  capability llm
  
  state local history: List[Message] = []
  
  behavior chat(user_message: String): String {
    let response = perform llm.complete(user_message)
    history = history ++ [Message { role = User, content = user_message },
                           Message { role = Assistant, content = response }]
    response
  }
}
```

## D.3 Storage to Persistent Actor Migration

### Before (v1.x)

```nulang
actor VisitCounter {
  store visits: Int = 0
  
  behavior increment() {
    visits = visits + 1
  }
}
```

### After (v2.0)

```nulang
persistent actor VisitCounter {
  state durable visits: Int = 0
  
  behavior increment() {
    visits = visits + 1
  }
}
```

## D.4 Tool Registration Migration

### Before (v1.x)

```nulang
agent Calculator {
  tool add(a: Int, b: Int): Int {
    a + b
  }
  
  tool multiply(a: Int, b: Int): Int {
    a * b
  }
}
```

### After (v2.0)

```nulang
actor Calculator {
  capability llm
  
  tool add(a: Int, b: Int): Int {
    a + b
  }
  
  tool multiply(a: Int, b: Int): Int {
    a * b
  }
  
  behavior calculate(query: String): String {
    let response = perform llm.tool_call(
      user = query,
      tools = [add, multiply]
    )
    response.text
  }
}
```

## D.5 Cluster Configuration Migration

### Before (v1.x)

```nulang
config cluster {
  nodes = ["node1:8080", "node2:8080", "node3:8080"]
  replication = 3
}
```

### After (v2.0)

```nulang
config cluster {
  seed_nodes = ["node1:8080", "node2:8080", "node3:8080"]
  replication_factor = 3
}
```

## D.6 Summary of Keyword Changes

| v1.x Keyword | v2.0 Equivalent | Notes |
|-------------|-----------------|-------|
| `agent` | `actor` with `capability llm` | Agents are now actors with LLM capability |
| `memory` | `state local` | Use `state` with appropriate model |
| `store` | `state durable` | Durable state replaces external storage |
| `on message` | `behavior` | Behaviors replace message handlers |
| `reply` | Return value | Behaviors return values directly |
| `tool` | `tool` (unchanged) | Tool declaration syntax unchanged |
| `prompt` | `perform llm.complete()` | Effects replace direct LLM calls |
| `agent.llm` | `perform llm` | Effect syntax replaces agent method calls |

## D.7 Deprecation Timeline

| Version | Action |
|---------|--------|
| v1.6 | Deprecation warnings for v1 keywords |
| v1.7 | Migration tool provided (`nulang migrate`) |
| v1.8 | v1 keywords deprecated, opt-out via flag |
| v1.9 | v1 keywords removed |
| v2.0 | Only v2 syntax supported |

## D.8 Migration Tool

Nulang 2.0 includes a migration tool that automatically converts v1.x code:

```bash
$ nulang migrate --input src_v1 --output src_v2
Migrating 42 files...
- Converted 5 agents to actors
- Converted 8 store declarations to durable state
- Converted 12 message handlers to behaviors
- Generated 0 manual review items

Migration complete. Review src_v2 before committing.
```

The migration tool handles the common cases. Complex migrations may require manual review.

---

This specification is a living document. As Nulang evolves, new features, refinements, and clarifications will be added. Feedback from implementers and users is essential to ensuring that Nulang 2.0 realizes its design goals.

---

# Appendix E: Known Design Tensions Under Review for v2

The following are **documented, intentional behaviors of the current implementation** — not bugs, and not changed by this specification. They are recorded here because each is a coherence hazard that should be resolved before the next frozen-tier bump (per `GOVERNANCE.md`, frozen-tier surface cannot change afterward). No language behavior is altered by this list.

1. **`!` has four meanings.** Logical not (prefix `!x`, `src/parser.rs` `prefix_precedence`), message send (`a ! b(args)`, Pratt postfix, `src/parser.rs:2466`), effect-row annotation (`fn f() ! {IO}`, `src/parser.rs:663`), and error-type annotation (`fn f() -> T ! E`, same site). All four are Stable-tier surface today; any future disambiguation is a breaking change and must happen before the next freeze.
2. **`&` has three meanings.** Prefix borrow / reference creation (`&x`, capability `ref`, `src/parser.rs` `prefix_precedence`), bitwise AND on `Int` (infix, `PREC_BITAND`, `src/parser.rs:42`), and the `&&` logical-and spelling family (`and`). A reference type in type position is also written with a capability sigil (`&val T`, §3.9.2).
3. **Bitwise operators bind tighter than arithmetic.** `&` (level 11), `^` (12), and `|||` (13) all sit above `+`/`-` (level 8) and `*`/`/`/`%` (level 9) in the Pratt table (`src/parser.rs:30-46`), so `1 + 2 & 3` parses as `1 + (2 & 3)` — the reverse of C-family convention. This is a documented quirk (§2.6) preserved for compatibility; parentheses are recommended when mixing arithmetic and bitwise operators.
4. **Division by zero yields `nil`.** Both integer and float division/modulo by zero evaluate to `nil` rather than trapping or returning an `Error` (`src/vm.rs` `step_idiv`, §2.6.1). This interacts awkwardly with the `! E` error-type and `?` propagation surface (item 1): an arithmetic failure is silent and type-invisible, while every other failure mode in the language is an explicit value.

**Recommendation:** items 1–4 should each get an RFC resolution (keep, rename, or re-precedence with a migration) before the next frozen-tier bump; afterwards they become unchangeable under the tier contract.

---

*End of Nulang Language Specification v2.0*
