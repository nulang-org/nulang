# Nulang: Plan to Production-Ready, World-Class

> **Status:** Draft, unratified.
> **Author:** planning artifact, not a governance document.
> **Scope:** the sequence of work to move Nulang from `0.1.0` alpha to a
> language a serious team can run in production and that stands up to
> comparison with Erlang/OTP, Rust, Elixir, and Pony on the axes each of
> those languages already owns.
> **Relationship to governance:** this file is descriptive. Each Frozen or
> Stable-tier change it names still requires an RFC per `GOVERNANCE.md` §4.

---

## Thesis

Nulang today has a coherent design, unusually strong format-stability
discipline for its age (RFCs 0001/0002), and roughly 70% of a production
implementation. The remaining 30% is neither exotic nor speculative: it is
closing gaps between advertised and implemented behavior, generating
evidence for claims that are currently unverified, delivering the
self-hosting story the frozen-format promise depends on, and getting the
first external user. Nothing on the critical path is a research problem.

Of the axes named in Scope above, the distributed actor runtime is where
the design has the most unclaimed advantage and the least proof. No
competing system ships CRDTs, capability-typed messages, and algebraic
effects as first-class actor-state primitives together; Erlang/OTP's
default distribution hits a 60-200 node full-mesh ceiling before requiring
a third-party topology library (Partisan), Akka/Pekko's split-brain
resolver and cluster sharding are mature but bolted onto a JVM actor model
with no CRDT or capability story, and Orleans' virtual actors get
automatic placement and distributed transactions but no CRDTs and no
capability-checked messaging. Nulang's distributed runtime today is a real
TCP/gossip mesh with genuine delta-state CRDT sync and full local
Erlang-style supervision — but it is unauthenticated by default, has no
split-brain handling, does not supervise or fail over across nodes, and
leaks CRDT tombstones forever. Phase 5 below treats closing that gap as
the primary differentiating ambition, not one item competing for attention
among Phases 1-4's broader production-readiness work.

## Principles

1. **Truth-in-advertising precedes new features.** No new surface until
   every claim in `README.md`, `LAUNCH.md`, and `CHANGELOG.md` is either
   implemented or removed. A user hitting a `not yet supported` error on
   an advertised flag is worth −100 users.
2. **Evidence, not estimates.** Every performance claim ships with a
   criterion benchmark under CI regression tracking. Every correctness
   claim ships with a test, a proof, or a conformance case.
3. **Deprecate before delete.** Once past the current alpha, every removal
   goes through the two-major-version deprecation cycle in
   `GOVERNANCE.md` §6, even for Experimental-tier surfaces where possible.
4. **One implementation, then two.** Multi-implementation credibility is a
   Phase 4 goal, not a prerequisite. The single implementation must be
   trustworthy first.
5. **Small binaries stay small.** The `no-default-features` build is the
   longevity floor; every new dep is checked against it.
6. **The steward is a bottleneck.** Every phase specifies which work is
   delegable, and the plan prefers deep specialists (fuzzing, formal
   methods, package registry) over generalist contributors.
7. **Kill criteria are explicit.** Every phase has a "stop and re-plan"
   trigger. Sunk-cost is not a strategy for a 200-year language.

---

## Current state (verified 2026-08-01)

| Signal | Value |
|---|---|
| Crate version | `0.1.0` |
| Language version | `1.0.0-frozen` (RFCs 0001, 0002) |
| Rust source | ~89k lines, single crate + `nulang-ai` workspace crate |
| Tests | 1490+ (`cargo test`), 1541+ target per `RELEASE_CHECKLIST.md` |
| CI matrix | build/test/release/wasm/minimal/lint/lean/package-smoke |
| Direct deps | 72 |
| Transitive deps | 472 (verified `Cargo.lock` `grep -c '^name = '`, 2026-08-14; was 504 before a 2026-08-02 libsql feature trim dropped the unused tonic/axum gRPC stack, then 468, already stale before that) |
| Formal proofs (Lean 4) | Core type soundness **proved** (`progress`/`preservation`/`type_soundness` machine-checked 2026-08-14); capability lattice proved (5/6 theorems — `linear_at_most_once` remains `sorry`, needs split-context refinement); effects are vacuous `True` stubs, not proofs |
| Conformance suite | 300 behavior cases + grammar cases |
| Bootstrap self-hosting | Stage 13; not yet self-compiling |
| Benchmarks | `benches/` uses criterion (7 files, 404 lines); no CI regression tracking |
| DST | `src/dst.rs` seed present (265 lines); not integrated into CI |
| Fuzzer | `src/fuzz.rs` present (412 lines); runs in `cargo test` |
| Shipped release binaries | None in repo evidence |
| External users | None known |
| Distributed cluster ceiling | full-mesh heartbeats + TCP connections (O(N) per node, O(N²) cluster-wide); gossip membership payload capped at 256 entries — practical ceiling in the tens of nodes, the same class of limit Erlang's default distribution hits before requiring Partisan |
| Cluster transport security | plaintext, unauthenticated by default; `TlsConfig::SelfSigned` exists (`src/runtime/network.rs`) but zero call sites in the entire codebase ever construct one — `enable_distribution` is only ever called with `tls_config: None` (sole caller: `src/runtime/tests.rs:2961`) |

Unfinished implementation lines counted from `not yet implemented` /
`not yet supported` markers in `src/`:

- `src/vm.rs:4553-4630` — 7 interpreter opcodes trap (`ConstL`, `Pop`,
  `Switch`, `Alloc`, `TupleL`, `Unpack`, `Copy`).
- `src/aot/codegen.rs:797-1108` — ~15 MIR constructs unsupported in the
  AOT native backend (all effects, actors, spawn, send, ask, receive,
  FFI, state, capability check).
- `src/mir_wasm.rs:322`, `src/wasm_runtime.rs:149,187` — WASM handler
  emission is a nil-drop; `host_read` and `host_dispatch` are stubs.
- `src/fmt.rs` — formatter now covers every `Decl`/`Expr` construct
  (workflow, agent, class, impl, let-binding, given, effect, module, import,
  extern, database, crdt, state_machine, named handler, record type,
  spawn/handle/receive/emit/migrate/cap-annotate/type-annotate), round-trips
  idempotently; 9 unit tests (2026-08-09).
- `src/typechecker.rs:274-284` — `opaque` nominal types are transparent.
- `Cargo.toml` — `simd-experimental` feature removed (zero code references); `quic-experimental` removed 2026-08-05 (unwired, incompatible handshake, tokio runtime overhead).
- `src/runtime/mod.rs:2773` — WASM component runtime path is a stub.

---

## Phase 0 — Truth-in-Advertising (weeks 0–4) — **COMPLETE 2026-08-01**

**Goal.** Every claim in the repo is either implemented, gated behind an
Experimental warning, or removed. No user hits an unimplemented path on
a documented flag.

**Actual outcome vs. original scoping below:** the single largest item
wasn't on this list. `--no-default-features` didn't compile at all —
`crate::ai::*` (LlmRequest, EpisodicMemory, SupervisorTeam, Pipeline,
ToolSchema, ...) was used unconditionally across `bytecode.rs`, `hir.rs`,
`hir_lower.rs`, and 6 `runtime/` files despite living behind
`#[cfg(feature = "ai-runtime")]`. This directly contradicted principle 5
("small binaries stay small") and the CI `minimal-build` job's own
stated purpose. Fixed by moving `ToolSchema` to a new core module
(`src/tool_schema.rs`, unconditional — it's core language surface per
RFC 0010 §C.2, not AI-specific) and gating every genuinely-AI function/
field/match-arm behind `ai-runtime` (~50 sites in `runtime/mod.rs` +
`runtime/actor.rs`, plus `agent.rs`/`ai_registry.rs`/`llm.rs`/
`supervisor_registry.rs` wholesale, plus 31 tests). `suspend_enabled`
moved off the AI-only `LlmState` onto `Runtime` directly — it wasn't
LLM-specific, core receive-wait suspension read it too. Verified: all
four feature configs (default/no-default/all-features/wasm-backend)
compile with zero warnings and pass their full test suites.

Also found and left unfixed (confirmed pre-existing on clean `HEAD`,
orthogonal to this phase's scope): `cargo clippy --all-targets -- -D
clippy::correctness` fails on a `clippy::approx_constant` hit in
`integration_tests/mod.rs:2278` and all 7 files under `benches/` fail
to compile under `--all-targets`. CI's lint job is red on `main`
independent of anything in this phase. Not fixed here — real but
unrelated to truth-in-advertising; needs its own pass.

**Deliverables.**

1. **Restrict advertised backends to what they support.** ✅ Revised
   decision after checking actual behavior: AOT already fails loudly
   with a specific "X is not yet supported in the native backend, use
   --backend bytecode" message per unsupported construct (verified —
   not a silent failure). The real gap was `--help` not saying so
   upfront. Fixed `--help` and the CLI doc comment to state native's
   (pure-functional only) and wasm's (IO.print/read only) scope before
   a user picks the flag, instead of adding a redundant runtime warning
   on top of an already-clear error.
2. **Interpreter opcode gap closure.** ✅ Confirmed all 7 opcodes
   (`ConstL`/`Pop`/`Switch`/`Alloc`/`TupleL`/`Unpack`/`Copy`) are
   unreachable from every current codegen path (zero matches across
   `mir_codegen`/`hir_lower`/`mir_lower`/`aot`/`mir_wasm`). Rewrote the
   error messages from "not yet implemented" (reads as a broken TODO)
   to state what each is reserved for and what already covers its use
   case. Added `test_no_codegen_path_emits_reserved_opcodes`
   (`integration_tests/mod.rs`) as CI-enforced proof, not just a
   comment claim.
3. **Formatter completeness.** ⏸️ Deferred, as flagged in the original
   scoping note. Genuinely multi-day: 5 distinct AST shapes need real
   formatting logic, not a quick fix. Tracked as its own follow-up.
4. **`opaque` nominal types.** 🔄 Revised after investigation: activating
   enforcement naively would have made opaque types permanently
   uninstantiable (verified: `mgu`'s current transparent unification is
   the *only* construction mechanism anywhere in the pipeline — no
   synthesized constructor, no cast operator). Real fix needs module-
   defining-scope tracking that doesn't exist in `TypeChecker` — new
   feature work, not a Phase 0 gap-closure. Also verified zero public
   docs (README/LAUNCH/CHANGELOG/SPEC2.md) claim opaque types work, so
   there was no truth-in-advertising violation to begin with. Tightened
   the code comment to state the zero-enforcement status unambiguously
   instead.
5. **Dead feature flags.** 🔄 Revised after investigation:
   `quic_transport.rs` is a complete, compiling `NetworkTransport` impl
   (TLS via rcgen, node-id handshake) — not the "empty stubs, panics in
   bind()" the stale Cargo.toml comment claimed. Deleting it would have
   destroyed working code to match a false comment. Real bug: the
   feature flag didn't gate its own `quinn` dependency (compiled into
   every build regardless). Fixed: `quinn` now `optional = true`, gated
   by `quic-experimental`; module doc states the honest unwired/
   untested caveat; stale Cargo.toml/CHANGELOG claims corrected.
   `simd-experimental` was genuinely 100% orphaned (zero code
   references anywhere) — deleted outright, zero risk. Also removed
   unused `multihash` dependency (never imported anywhere).
6. **README/LAUNCH audit.** ✅ Found and fixed real corruption: an
   orphaned table fragment (examples 15-17 with no header row) plus a
   dangling half-sentence, debris from an earlier edit. Fixed 3 stale
   "11 verified examples" mentions → 17 (actual count). Fixed stale
   "1490+ tests" → 1550+ (measured: 1554). Added the AOT backend's
   actual scope to Feature Highlights (previously unmentioned there).
   `examples/README.md` was itself missing rows for 16/17 — fixed.
   `LAUNCH.md` was already accurate; no changes needed.
7. **Release checklist enforcement.** ✅ `verify_implementation.py`
   claimed (via AGENTS.md) to run `cargo test` — it never did;
   `check_warnings()` only ran `cargo check --tests` (compiles, never
   executes). This is exactly why it never caught item 0 below. Added
   `run_tests()` (`cargo test --lib`), and extended `check_warnings()`
   to cover all three CI feature configs instead of just default.
0. **[Not originally scoped] `--no-default-features` didn't compile.**
   ✅ The dominant item this phase actually did — see "Actual outcome"
   above. `crate::ai::*` used unconditionally outside its feature gate
   across bytecode.rs/hir.rs/hir_lower.rs/6 runtime files. Fixed by
   moving `ToolSchema` to core (`src/tool_schema.rs`) and properly
   gating everything AI-specific (~50 sites + 4 whole modules + 31
   tests) behind `ai-runtime`, including moving `suspend_enabled` off
   the AI-only `LlmState` onto `Runtime` (core receive-wait needed it
   too — it was never actually LLM-specific).

**Acceptance — met.**
- `cargo test --lib` green on all 4 feature configs (default 1554,
  `--no-default-features` 1443, `--all-features` 1586, `wasm-backend`
  1586) — 0 failures throughout.
- `verify_implementation.py` exits 0 end-to-end (~43s), now actually
  running the test suite it claimed to run.
- `cargo check --tests` zero warnings on all 3 CI feature configs.
- `cargo fmt --check` clean throughout.
- README.md/examples/README.md/docs/GETTING_STARTED.md: every stale
  count and the corrupted fragment fixed; every remaining Feature
  Highlights bullet now matches verified behavior.

**Non-goals — held.** No new language features. No new backends
(AOT/WASM scope was clarified, not expanded). No performance work.

**Kill criteria.** If any bullet takes >2× its estimate, land the
downgrade (remove the surface) rather than the fix, and open an RFC for
the restoration. This phase must complete on schedule.

---

## Phase 1 — Correctness Floor (weeks 4–12)

**Current state (in progress, verified 2026-08-02):** 4/8 deliverables
done (fuzzer maturation, benchmark harness including regression
gating, doc-example verification extended), 3/8 substantially wired
this extended session (DST, persistence recovery, one real chaos-suite
test), bullet 5 at 239/300, bullet 7 partially addressed (phrase
cleanup, a unit-test verification layer for existing structured-error
fields, plus two arity-mismatch construction sites converted from
hollow to populated). 9 real runtime/tooling bugs found and fixed
(one, RFC 0008 migration contracts, was a false Stable-tier claim —
corrected in SPEC2.md), 12 more found and documented for follow-up
(including a real compiler SIGABRT on large functions and two
behavior-dispatch surprises), plus a dozen SPEC2.md/GOVERNANCE.md/
CHANGELOG.md truth-in-advertising corrections. 42 commits this
session. **Update 2026-08-13 (follow-up sweep):** three of the
documented follow-up bugs closed with regression tests — the
single-arg `perform Timer.sleep(ms)` workflow hang (the wake resumed
the PerformAsync but re-installed the completed frame state as a fresh
suspension; now the resume result decides completion vs re-suspension,
mirroring the LLM path), and the `send remote`/`ask remote`
local-delivery fallback + `RAsk` result-register convention (both
pinned at the runtime/VM level). SPEC2.md and CHANGELOG.md updated;
see their entries for evidence. **Update 3 (2026-08-13):** the saga
compensation + workflow step-dispatch misindexing (documented follow-up
bug 3 / SPEC2 §10 issue #2) fixed — compensation pairs carry absolute
behavior indices and workflow bytecode_offsets are compressed to the
actor's own steps; and SPEC2 §10 issue #5 fixed — failing workflow
steps now record a durable `StepFailed` event surfaced by the CLI
(stderr + exit 1) instead of exiting 0 silently. **Update 2 (2026-08-13):** the RFC-0007
cross-node routing gap closed — `spawn@node` references (bare
actor-ref values) now route over the wire via a runtime reverse index
(id → node), with spawn-in-flight message queueing and placeholder→real
translation; `ask remote` routes by the same index and the VM `RAsk`
opcode now accepts actor-ref targets and stages args. SPEC2.md/CHANGELOG.md
updated; see the changelog entry for the test evidence.
- **[X] Bullet 1 (fuzzer maturation) — interp/JIT/AOT leg done.**
  `src/fuzz.rs` grew from panic-avoidance to real differential execution
  fuzzing (`differential_fuzz_one`): compiles a mutant, runs it
  interpreted, forces real JIT tier-up on the same VM instance, and
  compares against the AOT backend when it accepts the program. Building
  this surfaced and fixed three of its own false-positive bug classes —
  worth recording because they're exactly the kind of subtle harness bugs
  that make a "0 divergences" result meaningless if unaddressed:
  `Value::to_string_repr()` doesn't resolve pool-indexed or heap-pointer
  values, so raw comparison across independently-compiled backends is
  unsound (fixed via `is_safely_comparable` gating + reusing the VM's own
  `string_operand` resolver for the same-VM leg); `VM::step_count` is a
  lifetime counter that accumulates across repeated `run()` calls, so a
  step-limit-triggered safety abort trips at different cumulative counts
  on cold vs warm and must be compared by category, not exact text; and
  forcing JIT tier-up via a fixed repeat count is unbounded when a
  mutant's own body loops heavily, requiring a wall-clock warmup budget
  instead. `fuzz_differential_quick` (300 iter, default `cargo test`) and
  `fuzz_differential_extended` (30,000 iter, `#[ignore]`d) both currently
  pass with 0 divergences. **Not done:** WASM backend comparison leg;
  reaching the 10⁶/day CI-nightly or 4×10⁴/day per-PR scale (that needs a
  dedicated scheduled CI job, not a `cargo test` invocation — the seed for
  one exists in `fuzz_differential_extended` but the job itself isn't
  wired).
- **[X] Bullet 3 (benchmark harness) — done, including regression
  gating.** `benches/*.rs` was fixed to actually compile and run (they
  didn't before — see `8636b01`). CI runs `cargo bench` on every push
  to `main`, collects results via `scripts/collect_bench_results.py`
  (verified against real criterion 0.5.x output, not just the
  documented schema), commits them to `benchmarks/`, and now also runs
  `scripts/check_bench_regression.py` against them. GitHub Actions'
  shared runners have commonly-cited 20-50%+ run-to-run noise on
  wall-clock benchmarks, so this deliberately isn't the `>5%` flat
  threshold this bullet originally called for (that would fail pushes
  on noise, not regressions, which is worse than no gate) — instead
  each benchmark's own median + 6×MAD across a rolling 10-commit
  window sets its threshold (floored at 20%), with a 3-prior-sample
  minimum before a benchmark is gated at all. Verified against
  synthetic history covering a genuine 2× regression (correctly
  flagged), a value within a noisy benchmark's normal historical
  spread (correctly not flagged), and both sparse-history cases
  (correctly skipped rather than false-positiving). See
  `benchmarks/README.md` for the full methodology.
- **[X] Bullet 5 (conformance suite expansion) — 26 → 300 of 300
  target, DONE.** Corrected from this doc's original "52" (that was a file
  count — `.nula`+`.json` pairs — not a case count; the actual starting
  case count was 26). Five waves of parallel agents:
  - Wave 1 (7 agents): capabilities, effect-handler resume, effect rows,
    actor messaging/supervision, CRDT merge laws, pattern
    matching/error handling, persistence/event sourcing → +87 cases.
  - Wave 2 (4 agents): built-in effects inventory, distributed-messaging
    single-node behavior, stdlib collections/string (the real, working
    subset — see the bug list below), workflow steps/conditionals/
    parallel/sagas → +43 cases, +2 direct regression cases.
  - Wave 3 (3 agents): JSON serialization (the real stdlib/json.nula
    API, quite different from SPEC2.md's description — corrected),
    HTTP client/server (proved client+server can't coexist in one
    process rather than asserting it; corrected SPEC2.md's API
    description too), concurrency primitives (scheduler FIFO/priority
    ordering, IEEE-754 float determinism including exact tie-break
    cases, and the real nil-collapse/epsilon-equality boundary
    semantics) → +17 cases.
  - Wave 4 (3 agents): generics (§7.8), imports/modules (§7.6-7.7),
    visibility (§7.9), and the Phase-4-experimental typeclass surface
    — zero prior coverage on any of these → +32 cases, plus 3 more
    targeted at the structured-error diagnostic fields (bullet 7).
    Surfaced a compiler crash on sibling same-named module functions
    (fixed) and a runtime crash on constrained generic typeclass
    dispatch (documented).
  - Wave 5 (3 agents): two Stable-tier claims with zero prior coverage
    (RFC 0008 migration contracts, RFC 0009 organization primitives),
    plus MIR register spilling and actor lifecycle edges → +28 cases.
    The most severe finding of this session: RFC 0008's "Stable" tag
    was false — migration contracts parse and shallow-typecheck but
    are functionally inert, no trigger mechanism exists anywhere in
    the runtime (corrected in SPEC2.md). Also found a real compiler
    SIGABRT (stack overflow) on ~286+-statement functions, and two
    behavior-dispatch surprises (unknown behavior names silently run
    behavior 0; same-named behaviors across different actor types can
    collide) — both documented, not fixed given the blast radius
    (`send_message` is called pervasively).
  Every value captured from the real compiled binary, never guessed.
  **Closed out 2026-08-13:** the final push to 300 landed the remaining
  cases and surfaced a real parser bug. Running the suite against the
  binary showed 8 of the lineariso cases failing with
  "Expected capability (iso, trn, ref, val, box, tag, lineariso,
  linear), found lineariso" — `parse_capability` (the `:cap`
  annotation path) matched `lineariso`/`linear` as `TokenKind::Ident`
  even though the lexer emits dedicated `LinearIso`/`Linear` tokens
  (the parameter-capability path `try_parse_param_capability` had the
  correct match; the annotation path never did). Fixed by matching the
  tokens directly; all 8 cases pass. Then added two final cases to
  cross 300: `cap_22_downgrade_iso_trn_ref` (iso → trn → ref lattice
  accepted, value usable at the weakest capability) and
  `cap_23_downgrade_trn_val` (the read-only end of the lattice) —
  both captured from real binary output. Suite is 300/300 passing,
  `./conformance/run.py` green. The AI-runtime surface (agent/
  Pipeline/Supervisor/Debate) remains appropriately covered at the
  Rust integration-test layer instead — those declarations need a mock
  LLM provider the CLI binary doesn't expose, so they are not
  reachable by CLI-driven conformance cases (not a coverage gap in the
  same sense as the others).
- **Real bugs found and FIXED this session (not from a numbered
  bullet, but squarely "correctness floor" — every one of these was
  found by an agent whose actual assignment was writing conformance
  cases, verified independently before fixing, and pinned by a
  regression case or test):**
  1. **Top-level heap allocation silently failed for any actor-using
     program** (`7215088`, `ef1f451`). `RuntimeVmCallbacks` allocated
     new heap strings via `Runtime.vm.allocate_string` — a separate,
     lazily-created VM instance whose heap `main()`'s own top-level VM
     can't read back from — and `alloc`/`drop_ref`/`retain_ref`
     returned `None`/no-op whenever there was no *current actor*, which
     is always true at the top level. Confirmed against a real shipped
     example (`examples/supervisor_tree.nula` printed literal `nil`).
     Fixed with a dedicated `Runtime::main_heap`/`main_gc` fallback.
  2. **Importing two stdlib modules with a same-named function crashed
     the compiler** (`b3d6a2f`). `import stdlib::map` + `import
     stdlib::set` (both export `empty`/`contains`/`remove`/...) produced
     two same-named top-level functions with no collision check, which
     MIR's function-slot allocator can't handle — it failed deep in
     codegen with `internal: MIR function slot 0 left unfilled`. Fixed
     by detecting the collision at import-resolution time with a clear,
     actionable error instead.
  3. **Workflow-only programs never activated the actor runtime**
     (`2057900`). `main.rs`'s actor-detection matched `Decl::Actor`/
     `Decl::StateMachine` but not `Decl::Workflow`, so a program with
     only a `workflow` declaration ran on the stub-only standalone VM —
     every step silently never ran, no error. One-line fix.
  4. **LSP hover/autocomplete advertised effects that don't work**
     (`6d27037`). `STM`/`Async`/`Cost` shown with full example syntax
     despite zero implementation; `Net`/`Rand` didn't match their real
     names (`Http`/`Random`); `Spawn`/`Send`/`Receive`/`Migrate` shown
     as `perform`-able effects when they're actually keywords/opcodes
     with a parse error on that syntax. Removed the non-functional
     entries, corrected the misnamed ones.
  5. **Two sibling `module { }` blocks declaring a same-named function
     crashed the compiler** (`5e9430c`, extended session). Nested
     modules are purely a flattening/namespacing construct — `module
     Alpha { fn value() {..} } module Beta { fn value() {..} }` both
     land in the same flat `func_map`, and the second registration
     silently overwrote the first's slot mapping, leaving the
     first-reserved MIR function slot permanently unfilled: `internal:
     MIR function slot 0 left unfilled`. Same root-cause *shape* as bug
     2 above (silent name collision surfacing as an internal-error
     symptom far from the real cause) but a different code path
     (`mir_lower.rs`'s `reserve_decl`, not the resolver's import
     merging). Fixed with the same pattern: detect the collision where
     it happens, name it in the error.
  6. **Recovered actors silently lost per-field persistence-model
     tracking, breaking a second crash/recovery cycle** (`03ed058`,
     extended session). `Runtime::recover_actor` built a bare
     `Actor::new()` and never restored `Actor.state_models` (the
     `local`/`durable`/`event_sourced`/`crdt` map per field) — every
     field silently reverted to `local` after one recovery, meaning a
     *second* crash would have dropped `durable` fields from the
     snapshot entirely and stopped `event_sourced` fields from
     accumulating. Fixed by restoring `state_models` from the recovery
     module's `actor_metadata` alongside `bytecode_module`/
     `bytecode_offsets`. Verified with a new two-cycle recovery test.
- **Real bugs found and DOCUMENTED, not fixed this session (tracked
  for follow-up, all in `SPEC2.md` with full evidence):**
  1. `ask remote`/distributed `RAsk` returns the wrong value (the
     target's own actor reference) from a register-write mismatch
     between the local and remote `Ask` opcodes.
  2. `send remote`/`ask remote` silently drop their message single-node
     instead of using the local-delivery fallback that already exists
     and works for other distributed paths — just isn't wired to these.
  3. ~~Saga compensation indexes by whole-module declaration order, not
     the workflow's own steps — another `actor` declared before the
     `workflow` silently shifts which step's compensation runs.~~
     **Fixed 2026-08-13** (see SPEC2 §10 known-issue #2): compensation
     pairs carry the step's absolute behavior index, and workflow
     bytecode_offsets are compressed to the actor's own steps — a
     pre-declared actor can no longer hijack the first compensation or
     shift step dispatch. Pinned by
     `test_saga_compensation_ignores_non_workflow_actors`.
  4. Single-argument `perform Timer.sleep(ms)` suspends a step and
     never resumes it — a permanent hang, not an error. Only the
     two-argument durable form works.
  5. ~~`event_sourced` field reconstruction during recovery is a bare
     count of persisted events, never running the field's `apply`
     handler against the event's args — correct for a plain counter,
     silently wrong for any field with a non-trivial `apply` handler~~.
     **Fixed:** `emit_event` persists the post-apply field value and
     recovery restores it (see SPEC2 §9.6;
     `test_event_sourced_apply_handler_recovery`).
  6. ~~A constrained generic function using a typeclass bound on a
     type-variable receiver type-checks but crashes at runtime ("Not a
     function: nil") — the dictionary-passing transform only resolves
     literal receivers~~. **Fixed:** `DictKind::Param` at HIR (see
     SPEC2 §1; `typeclass_06` passes).
  7. ~~Recursive generic ADTs cannot be constructed~~. **Fixed**
     (rigid-variable handling; `generics_03`/`07` pass).
  8. ~~Generic function type parameters are not skolemized in the
     function body~~. **Fixed:** rejected at the definition
     (`generics_08` expects the type error).
  Also corrected: `SPEC2.md` §4.6 (built-in effects table — see bug 4
  above), §12.4 (distributed message routing — see bugs 1-2 above, plus
  a separate correction: `monitor`/`link`/`exit` were undersold as
  "planned" when they're fully implemented and conformance-tested),
  Chapter 10 (workflow known-issues list — see bugs 3-4 above, plus a
  deprecation note per RFC 0004), Chapter 14 examples (stdlib argument
  order/naming mismatches found by the `StdlibCollectionsString` wave;
  Chapter 14 was already headed "— Planned" so this didn't need the
  same "Stable-tier false claim" severity of fix CRDT got).
- **[X] Bullet 6 (doc-example verification) — closed 2026-08-13.** The
  default CI invocation was red and the SPEC2/README/`///`-comment
  halves of the bullet were unwired. Now all sources are scanned by the
  default run (the `NULANG_DOC_VERIFY_INCLUDE_ROOT` opt-in gate is
  gone): the Astro docs site, `SPEC2.md`/`README.md`/
  `docs/GETTING_STARTED.md`/`docs/TUTORIAL.md`, and `///` doc comments
  in `.nula` sources. 16 docs-site blocks and 41 SPEC2 blocks were
  rewritten against the current compiler (stale syntax, unbound prose
  references, type/fn-type spelling drift, `Err`→`Error`,
  recursive-ADT tuple payloads, pre-`then` `if`, `::` imports, `ref`/
  `state`/`rec` reserved-word collisions, unclosed fence, `&`-capability
  truth). Genuine non-programs are skipped by explicit markers only:
  `// fragment` lines and "— Planned" headings (now detected at `###`
  depth too). Default run: 132 passed / 0 failed / 54 skipped.
  The pass surfaced seven REAL language gaps, all now documented in
  SPEC2/PLAN (see "Gaps found by the doc pass" below).
- **[X] Bullet 7 (structured error quality) — closed 2026-08-13.** The
  phrase cleanup was already done ("not yet supported" gone from
  user-facing errors); this session verified the hollow-helper sweep is
  complete — zero `type_error(msg, …)`/`cap_error(msg, …)`/
  `effect_error(msg, …)`/`parse_error(msg, …)` call sites remain in the
  codebase — and documented the deliberate ceiling: the dynamic
  variants (`LexError`/`FFIError`/`RuntimeError`/`VMError`/
  `PythonError`/`PackageError`) stay `{msg, span}` because they are
  message-driven without an expected-vs-found shape (extending them was
  assessed lower-value by the 2026-08-02 pass), and the
  `similar_names`/`explanation` fields fill the `suggestion` role under
  a different name.
- **[X] Bullet 2 (DST) — single-node message-passing + timer determinism
  landed, seed-sweep invariant test landed; cluster/network determinism
  landed 2026-08-14 (see the "cluster/network determinism" addendum at
  the end of this bullet).** `src/dst.rs` was not even part of the
  compiled crate (`mod dst;` was missing from `src/lib.rs` — its 4
  tests had never run). Fixed that first, then added
  `Runtime::run_scheduler_deterministic`/`pick_ready_actor_deterministic`:
  actor selection driven by a seeded RNG over the sorted ready-set,
  reusing `step_actor` unchanged (same VM/GC/persistence machinery the
  production scheduler drives). Scope, by design: pure message-passing
  determinism only — does not drive the timer wheel, cross-shard
  messages, or LLM completions, all of which key off wall-clock reads.
  3 new tests verify same-seed same-sequence selection, real
  quiescence with correct final actor state, and step-limit-exceeded
  reporting for a run that hasn't settled. Then (2026-08-13) landed the
  seed-sweep invariant test, the core "N seeds, fail on any invariant
  violation" deliverable at CI scale:
  `test_dst_seed_sweep_at_most_once_delivery` runs a counter+decoy
  program under 2000 seeds (200 interleaved messages each) asserting
  EVERY run (a) reaches quiescence — never `StepLimitExceeded`
  (deadlock/livelock signal) — and (b) the counter reaches exactly 200
  (AtMostOnce: no lost or double-delivered messages). Validated the
  test has teeth by injecting a message-drop into the counter behavior
  and confirming it fails; ~21s for the full sweep because the
  deterministic path never sleeps. Timer determinism (same session):
  when no actor is ready but the timer wheel is non-empty and a virtual
  clock is installed, `run_scheduler_deterministic` advances the clock
  to the next deadline and re-ticks, so timer-armed programs (send_after,
  timed receive-waits) make deterministic progress instead of Quiescing
  forever with their timers pending; clock advances count toward
  `max_steps` so timer-rearming loops stay bounded. WITHOUT a virtual
  clock the old contract holds (timer programs Quiesce with timers
  pending). `test_dst_timer_fires_under_virtual_clock` asserts both
  sides. The 10⁴-seeds-per-commit CI job is now wired (2026-08-14):
  `.github/workflows/dst-nightly.yml` runs nightly (cron 04:00 + manual
  dispatch) with `NULANG_DST_SEEDS=10000`; the seed counts are
  env-configurable via `src/dst.rs::dst_seed_count` (in-suite defaults:
  2000 single-node / 50 cluster / 30 cross-shard), so the same tests
  scale from CI-fast to nightly-depth without editing code.

  **Addendum (2026-08-14) — cluster/network determinism landed.**
  The remaining gap — cross-shard channels and the real transport
  keying off wall-clock reads — is closed:
  - **Transport injection.** `Runtime::enable_distribution_with_transport`
    accepts any `Box<dyn NetworkTransport>`; `enable_distribution` (TCP)
    became a wrapper. DST tests run REAL `Runtime`s over the existing
    in-memory `DeterministicNetworkTransport` (zero threads, zero sleeps).
  - **Seeded cluster RNG.** `ClusterState::set_rng` — `pick_gossip_targets`
    and `repair_active_view` draw from a seeded `Box<dyn RngCore>`
    (`DeterministicRng` implements `rand_core::RngCore`) when installed,
    so gossip/repair picks are bit-reproducible; `OsRng` remains the
    production path.
  - **Cross-shard determinism.** `run_scheduler_deterministic` now drains
    `cross_shard_rx` at the top of every iteration (mirroring the
    production scheduler), so `new_sharded` runtimes run deterministically
    too. The with-RNG variant (`run_scheduler_deterministic_with_rng`)
    lets a harness interleave nodes and actors from one seeded stream.
  - **`DeterministicCluster` harness** (`src/runtime/cluster_dst.rs`,
    test-gated): N real `Runtime`s over the in-memory fabric, per-node
    virtual clocks advanced in lockstep, one master RNG permuting node
    order per round + driving each node's actor selection, heartbeat wire
    timestamps drawn from the virtual clock (byte-identical packets).
    4 new tests: same-seed bit-reproducible evolution, a 50-seed remote
    AtMostOnce delivery sweep over the fabric, a 3-node
    partition→Failed-detection→heal→remote-delivery scenario through the
    REAL failure detector, and a 30-seed cross-shard delivery sweep.
    All 8 DST tests (including the 2000-seed single-node sweep) run in
    ~45s in-suite.
  - **CRDT-sync-race scenario (2026-08-14, bullet-2 deliverable).** The
    harness drives `rt.sync_crdts()` per round — CRDT replication is a
    Rust-embedder API by documented design (SPEC2 §12.5: no `.nula`
    surface, not auto-driven by the production loop until an RFC wires
    `state crdt`; the harness models the embedder calling the API on the
    cluster cadence, which is exactly how nulang-cloud drives it).
    `test_dst_cluster_crdt_convergence_seed_sweep`: 40 seeds, two nodes;
    a GCounter minted on A must appear on B via the round-1 full-state
    sync, then both nodes increment their LOCAL replicas interleaved
    with sync rounds (seeded node order decides which side's deltas
    ship first); every seed converges both replicas to the summed total
    (GCounter is commutative — no lost update under any interleaving).
  - **Node-crash scenario (2026-08-14, bullet-2 deliverable).**
    `test_dst_cluster_crash_restart_seed_sweep`: 20 seeds, three nodes;
    node 2 hard-crashes (dropped from the pump, links cut) and the
    survivors mark it `Failed` through the REAL virtual-clock failure
    detector; the node restarts as a FRESH `Runtime` (same node id,
    fresh state — the harness's `crash_node`/`restart_node` mirror the
    real-TCP crash/rejoin test) joining through a survivor; the cluster
    reconverges to full `Healthy` and a remote message delivers to an
    actor on the restarted node. This is the seed-sweepable, sleep-free
    counterpart of `test_three_node_cluster_survives_hard_node_failure_and_rejoin`.
  - **Message-reorder scenario (2026-08-14, bullet-2 deliverable).**
    `DeterministicNetworkTransport` gains a bounded-adjacent-reorder
    mode (`set_reorder`/`flush_held` on the `NetworkTransport` trait,
    default no-ops): packets on a (from,to) pair are delivered with
    consecutive pairs swapped (P2 before P1) — nothing is lost or
    duplicated, only delayed one slot; the harness
    (`DeterministicCluster::set_reorder_all` + per-turn flush) enables
    it on every link from the first packet. `test_dst_cluster_message_reorder_seed_sweep`:
    25 seeds, three nodes; the cluster must FORM under reordered
    heartbeats/gossip/acks (order-independent merges), a 30-message
    remote burst delivers exactly 30 (AtMostOnce under reorder), and
    GCounter replicas converge to the summed total under reordered
    delta sync.
  - **GC-during-send scenario (2026-08-14, bullet-2 deliverable).**
    The deterministic scheduler now pumps GC on the production cadence
    (deferred frees every `GC_PUMP_INTERVAL` steps, foreign-ref
    decrements + deferred retry at quiescence) — the DST path previously
    never applied `process_gc_ops`, so heap-churn scenarios could not
    run. `test_dst_gc_during_send_seed_sweep`: 60 seeds; a builder
    actor allocates nested array trees on its own heap, sends each to a
    receiver (in-flight foreign bump + receiver hold), then releases
    its local ref (deferred free); a churn actor allocates+frees blocks
    so the seed permutes the GC interleaving. Every seed: all messages
    deliver with intact contents (no premature free, no refcount
    imbalance), all `MESSAGES × (K+1)` tree objects stay alive on the
    builder (held by the receiver), and the receiver heap holds only
    its cycle-detector sentinel.
- **[X] Bullet 4 (chaos suite) — three real topologies landed + virtual-clock
  determinism, 5-node/split-brain/asymmetric done, seed-scale CI wired
  2026-08-14 (dst-nightly.yml, `NULANG_DST_SEEDS=10000` over the same
  `DeterministicCluster` harness — see the bullet-2 addendum).** `test_three_node_cluster_survives_hard_node_failure_and_rejoin`
  (`src/runtime/tests.rs`, extended session) drives 3 real `Runtime`
  instances over real loopback TCP: kills a node's transport hard (no
  graceful leave), confirms the survivors detect the failure via the
  real heartbeat-timeout/suspicion state machine, confirms they keep
  doing real cross-node work together (not just membership-table
  bookkeeping), then confirms a fresh node can rejoin.
  `test_three_node_cluster_survives_rolling_restart_of_every_node` (the
  `fix/distribution-address-discovery` branch) extends the shape to a
  full rolling restart of every node, gated on address convergence.
  Then this extended session added the partition-injection primitive
  (`NetworkTransport::set_partition`, real drop in `TcpTransport::send`
  and the DST transport — outbound packets to the listed peers silently
  vanish exactly like a firewall) plus **virtual-clock determinism**:
  per-node `VirtualClock`s advanced in lockstep (`Runtime::advance_time`
  re-syncs `ClusterState`'s clock every call, so the PLAN's earlier
  "cluster clock unlinked from runtime clock" note is stale — the two
  were wired together). `test_three_node_cluster_split_brain_detects_and_heals`
  partitions {A,B}|{C}, asserts both sides mark the other `Failed`
  through the REAL failure detector while each sub-cluster stays
  internally `Healthy`, then heals via the probe path and delivers a
  remote message across the former boundary. `test_three_node_cluster_asymmetric_partition_detects_and_heals`
  asserts one-directional visibility (A sees B, B can't see A) with
  exactly the expected asymmetric `Failed`/`Healthy` outcome, then
  heals. `test_five_node_cluster_split_brain_detects_and_heals` covers
  the 5-node topology item. All three run in ~0.4 s total (vs ~25 s
  per real-wall-clock test) because failure detection fires in virtual
  time. Key harness lesson: membership converges via gossip in ~1 s
  virtual, but the failure detector only watches the ACTIVE view, which
  fills through the 5 s repair cycle — tests that inject a partition
  must first pump until `active_view` contains every peer
  (`active_views_converged`), or the partitioned side never watches the
  other side at all. The split-brain RESOLVER down-self path is now also
  covered end-to-end: `test_three_node_cluster_static_quorum_downs_minority`
  partitions {A}|{B,C} with `StaticQuorum{3}` (quorum 2) and asserts the
  isolated minority downs itself through the REAL runtime — the
  `ClusterAction::Down` handler shuts the transport down while local
  majority stays up and keeps delivering
  remote messages, and healing does NOT resurrect the downed node
  (operator restart is the recovery path). Writing that test surfaced a
  REAL cold-bootstrap bug: `ClusterState::tick` consulted the resolver
  from the very first tick, so a fresh seed node (which sees only
  itself, 1 < quorum) downed itself before join handshakes completed —
  the cluster-sim masked this by pre-seeding a full mesh and the unit
  tests by calling `handle_heartbeat` before `tick`. Fixed with a
  `has_seen_peer` gate: the resolver is consulted only after the node
  has ever received a heartbeat from any peer, so a node that has never
  contacted anyone is treated as bootstrapping, never as a partition
  victim. Seed-scale CI closed 2026-08-14:
  `.github/workflows/dst-nightly.yml` runs `NULANG_DST_SEEDS=10000`
  (`cargo test --lib dst_`) nightly + on dispatch — the same
  `DeterministicCluster` harness, at chaos depth, sleep-free.
- **[X] Bullet 8 (persistence recovery correctness) — full StateModel
  sweep done.** `Runtime::recover_actor` never
  restored `Actor.state_models` on the rebuilt actor, so every field
  silently reverted to `Local` after one recovery — a second crash
  would have dropped `durable` fields from the snapshot entirely.
  Fixed and verified with a new two-cycle recovery test. Separately,
  and NOT fixed: `event_sourced` field reconstruction during recovery
  is a bare count of persisted events, never running the field's
  `apply` handler against the event's args — correct for a plain
  counter, silently wrong for any field with a non-trivial `apply`
  handler (verified against the real compiled binary: an `apply`-driven
  counter reaches 9 with no crash, only 6 with a crash-and-recover
  between the same two messages). Root cause is architectural — apply
  handlers are inlined at each `emit` call site at compile time, no
  addressable bytecode unit recovery could re-invoke — tracked as
  follow-up, documented in SPEC2.md §9.6 and pinned by a regression
  test. `local`/`crdt` StateModels closed 2026-08-14: `local` fields
  reset to their declared initial value on recovery (never the
  pre-crash value, never unset) while a durable anchor proves recovery
  ran — `test_local_state_resets_to_initial_value_on_recovery`; and
  `crdt` fields survive crash+recovery through the same snapshot+
  journal path as `durable` (the documented current behavior — SPEC2
  §9.10, §12.5) — `test_crdt_state_recovery_behaves_as_durable_today`.

**Gaps found by the doc pass (2026-08-13, all verified against the
compiled binary).** The 41 rewritten SPEC2 blocks taught syntax the
compiler rejects; most were doc drift, but seven are genuine language
gaps now tracked:
  1. **Capability references are annotation-only beyond `ref`.**
     **CLOSED 2026-08-14.** `&cap expr` now constructs a reference with
     the requested capability for all eight capabilities; bare `&expr`
     stays `&ref`. The unique constructors (`&lineariso`/`&linear`/
     `&iso`/`&trn`) consume a bare-variable operand like `consume x`
     (second use rejected); shared constructors alias. Erased to a plain
     move at runtime; formatter prints `&cap`. SPEC2 §3.9 rewritten;
     CHANGELOG entry; parser/analyzer/integration tests + differential
     corpus.
  2. **Prelude types were unusable in annotations.** `let ok = Ok(42)`
     type-checked in every module while `fn f(x: Option[Int])` failed
     to parse ("Unknown type name") because the prelude's type decls
     are prepended to the AST *after* the user module parses. Fixed:
     every `Parser` now seeds the prelude's resolved `Option[T]`/
     `Result[Ok, Err]` into its imported-type cache (same machinery as
     `import stdlib::*`); local decls still shadow. Pinned by
     `test_prelude_types_resolve_in_annotations` and
     `test_local_type_shadows_prelude_in_annotation`.
  3. **`let rec` is unsupported** (SPEC2 §6.5 taught it); recursive
     local bindings are written as functions. **CLOSED 2026-08-14.**
     `let rec f(x) = ... in ...` already worked in expression position
     (`parse_let_rec_named`); the gap was module-level entry — the
     `parse_module_let` path failed on the parameter list ("Expected =")
     with the parser already past the `let` token, so `parse_module`'s
     zero-consumption expression fallback never fired. `parse_module_let`
     now rewinds to the `let` token when the name is followed by `(` and
     the expression path handles it. Pinned by parser + integration
     tests (module-level and fn-body recursion through the VM).
  4. **Bare single-effect rows (`! IO`) are rejected**; braces required
     (`! {IO}`). SPEC2 §4.5.1 claimed equivalence that did not hold.
     **CLOSED 2026-08-14 (doc side, deliberately).** `! Name` is the
     typed-error surface (`! Type`) — load-bearing (error-type tests,
     catch/fail); effect rows require braces, disambiguating the two.
     §4.5.1's stale "shorthand" prose removed; the example comment
     (braces required) is now the whole story.
  5. **Array/record alias bodies are rejected** (`type Buffer = [Int]`
     → "Expected variant name"); aliases expand only to variant/nominal
     shapes. **CLOSED 2026-08-14.** `parse_type_decl_variant_or_record`
     now routes any non-variant/non-record body through `parse_type()`
     into a `Decl::TypeAlias` (records already parsed; `type alias`
     already accepted arbitrary bodies — the bare `type X =` keyword
     path just never reached it). Primitive-named bodies (`type T = Int`)
     lex as `UpperIdent`, so they are special-cased to the alias path
     too. Variants (`Some(T) | None`) and records are untouched. Pinned
     by parser + integration tests.
  6. **`state`, `rec`, `ref` are reserved words** — SPEC2 examples used
     them as identifiers (now fixed); `let rec` and `let ref` cannot be
     written.
  7. **`in`-let scope spans exactly one trailing expression** — SPEC2
     blocks that chained statements after `in` were invalid (rewritten
     to sequential lets).

**Goal.** The language does what it says, provably, on the paths users
actually take. This is what makes 0.1.0 → 0.2.0 justifiable.

**Deliverables.**

1. **Fuzzer maturation.** Grow `src/fuzz.rs` from panic-avoidance to
   differential fuzzing: compile a mutant, interpret vs JIT vs
   (surviving) AOT/WASM backends, assert identical observable results
   or identical errors. Target: 10⁶ iterations/day in CI nightly,
   4×10⁴/day in per-PR CI. Any divergence is a bug.
2. **Deterministic Simulation Testing.** Wire `src/dst.rs` into the
   actor runtime. Deliverables:
   - `Simulator` replaces `Scheduler`, `NetworkTransport`, and the
     wall clock with deterministic fakes.
   - Message reorder, network partition, node crash, GC-during-send,
     and CRDT-sync-race scenarios expressed as seed-driven tests.
   - CI job runs 10⁴ seeds per commit, fails on any invariant
     violation (deadlock, lost message under `AtMostOnce`, CRDT
     divergence, supervision-cascade failure).
   - Any bug found is captured as a permanent regression test with
     its exact seed.
3. **Benchmark harness with regression tracking.** ✅ **Delivered.** Every
   criterion bench under `benches/` runs in CI on `main` pushes (not
   PRs — a full run of every group is too slow to gate every PR on);
   results are written to `benchmarks/` in the repo. Regression
   threshold is not a flat percentage (GitHub Actions' shared runners
   show 20-50%+ run-to-run noise, which a flat 5% cutoff can't survive)
   — `scripts/check_bench_regression.py` computes each benchmark's own
   median + 6×MAD across a rolling 10-commit window, floored at 20%, and
   requires ≥3 prior samples before gating a benchmark at all. Publishes
   measured numbers to replace the estimates in `PERFORMANCE_ANALYSIS.md`.
4. **Chaos suite for distribution.** Extends the DST harness with
   concrete cluster topologies: 3-node, 5-node, split-brain,
   asymmetric partition, rolling restart. Runs 10³ seeds per commit.
5. **Conformance suite expansion.** Grow `conformance/behavior/` from
   26 to ≥300 cases covering every Frozen and Stable surface — every
   built-in effect, every capability transition, every CRDT merge law,
   every supervisor restart strategy, every effect-handler resume
   shape. This is the executable spec that a second implementation
   would target.
6. **Doc-example verification.** `scripts/verify_doc_examples.sh` runs
   every code block in `docs/`, `README.md`, `SPEC2.md`, and every
   `///` doc comment. A doc block that doesn't compile+run fails CI.
7. **Structured error quality pass.** Every `NuError` variant carries
   `expected`/`found`/`suggestion` per the recent structured-errors
   work, verified by test. No error contains the phrase
   "not yet supported" — those become their own error variant with a
   documented workaround.
8. **Persistence recovery correctness.** DST-driven test: kill an
   event-sourced entity mid-journal, restart, assert state equals a
   from-scratch reconstruction. Repeat for every `StateModel`.

**Acceptance.**
- Differential fuzzing 0 divergences over 10⁶ seeds.
- DST 0 invariant violations over 10⁵ seeds spanning all cluster
  topologies.
- 300 conformance cases pass on the current runtime.
- Benchmark dashboard live; every claim in `PERFORMANCE_ANALYSIS.md`
  is either measured or removed.
- Version bump: 0.2.0 (crate), language version unchanged.

**Non-goals.** New language surface. Ecosystem work.

**Delegable to.** Fuzzing specialist (bullet 1). Distributed-systems
tester (bullets 2, 4). Docs contributor (bullets 5, 6). Steward retains
bullets 3, 7, 8.

**Kill criteria.** If differential fuzzing surfaces a Frozen-tier bug
(bytecode divergence, wire-format divergence, value-layout divergence),
freeze all new work and treat it as a Sev-1 until fixed. This is the
whole point of the Frozen tier.

---

## Phase 2 — Prove It Works (weeks 12–24)

**Current state (in progress, verified 2026-08-02):** 8/8 scoping areas
investigated this session; 3 concrete deliverables landed, 5 scoped and
deferred (all multi-day/infrastructure-gated, not something a single
session can responsibly rush). 5 commits this session.
- **Bullet 1 (formal semantics) — documentation corrected, real proof
  work still open.** Discovered `types.lean`'s three headline theorems
  (`progress`, `preservation`, `type_soundness`) were silently regressed
  to `sorry` by a Lean 4.16.0 compatibility-fix commit (`ac9ef5d`,
  2026-07-26) three weeks before this session — that commit's own
  message honestly disclosed "12 sorry warnings", but no downstream doc
  (`spec/formal/README.md`, `SPEC2.md`, `CHANGELOG.md`, `PLAN.md`) was
  ever updated to match; all four falsely claimed "0 sorries"/"proved"
  until corrected this session. Root cause identified and documented: a
  concrete variable-capture/context-ordering subtlety in the `weakening`
  lemma's naive-induction proof strategy (see
  `spec/formal/README.md`'s regression note). Added a CI sorry-count
  ratchet (`.github/workflows/ci.yml`) so this exact silent-regression
  pattern can't recur — previously CI only ran `lake build`, which
  passes even with sorries. **Landed 2026-08-14:** the soundness chain
  was actually re-proved — `progress`/`preservation`/`type_soundness`
  are machine-checked in `types.lean` (see `spec/formal/README.md`),
  and the CI sorry-ratchet baseline dropped from 9 to 1 (only
  `linear_at_most_once` remains, needing the split-context judgment).
- **[X] Bullet 2 (LinearIso must-use) — closed 2026-08-14.** Exactly-once
  (must-use) is enforced for `let`-bound linear values, with a
  transparent-rebind exemption (`let a = x` doesn't carry a second
  obligation) verified against all 6 existing lineariso conformance
  cases plus 8 new unit tests. Parameter-level must-use (a linear value
  already in scope, e.g. a function argument) is enforced and now
  verified END-TO-END through the compiled binary with 5 new conformance
  cases (cap_30–34): single use ok, double use rejected ("used after
  being consumed"), never used rejected ("lineariso bindings must be
  consumed exactly once"), explicit `consume x` discharge ok, and a
  `lineariso` BEHAVIOR parameter consumed once. Syntax: prefix param
  annotation (`fn f(lineariso x: Int)`); callers pass literals (val
  promotes) or `consume`-created values. Along the way the conformance
  suite surfaced and fixed a real parser regression from the gap-5 type
  routing: `Nil` (a primitive type name) was routed to the alias path,
  breaking `type Stream[T] = Nil | Cons(...)` — `Nil` is the canonical
  empty variant of a sum type and is now exempt from alias routing
  (`type_decl_body_is_alias`), pinned by a parser unit test. Full
  conformance: 305/305 (was 300/300 + 5 new; also stabilized the
  generics_08 stderr assertion that depended on internal fresh-var
  numbering, and updated workflow_09/11 expected contracts to the
  deliberate post-2d56e33 behavior — a failing saga step surfaces a
  diagnostic and exits nonzero).
- **Bullet 3 (backend traits) — verified already done, not a gap.**
  `src/backends/mod.rs`'s own header claims every trait
  (`JitBackend`/`WasmBackend`/`Transport`/`CryptoProvider`/
  `HttpProvider`/`ForeignInterop`) is "Wired"; spot-verified `VM` does
  genuinely hold `Option<Box<dyn JitBackend>>` and construct it through
  the trait. No work needed here.
- **Bullet 6 (release binaries) — verified already mostly done.**
  `.github/workflows/release.yml` builds 4 targets (Linux x86_64/
  aarch64, macOS x86_64/aarch64), strips binaries, SHA256-checksums,
  and publishes to GitHub Releases on tag push; `v0.1.0` is tagged.
  Gaps: no cryptographic code signing (checksums only), and a 5th
  target (Windows) is blocked on Windows support itself (bullet 5).
- **Bullet 7 (LSP hardening) — protocol-level gap closed 2026-08-14.**
  38 unit tests cover individual feature logic (inlay hints, completion,
  hover, workspace symbols, diagnostics); the previously-open
  "no protocol-level (tower-lsp test-harness) integration tests" gap is
  now closed with 6 tests driving the FULL JSON-RPC dispatch path —
  `tower_lsp::LspService` with real `Request` objects (the same service
  the stdio server runs), asserting request/response round-trips
  (initialize capabilities, hover signature, completion keywords,
  documentSymbol outline, shutdown/exit lifecycle incl. ExitedError
  after exit) AND the server->client notification stream
  (publishDiagnostics pushed on didOpen/didChange: empty for
  well-formed docs, parse-error severity-1 diagnostic for broken ones).
  In-process, no subprocess, `#[tokio::test]`; new direct deps
  `futures`/`tower-service` (both already in the lockfile). The 24-hour
  soak test against a large corpus remains open — it needs wall-clock
  time no single session has.
- **Bullet 8 (dependency audit) — real, verified progress.** Found
  `libsql`'s `default-features` pulled in `replication`+`sync`, which
  drag in the entire `tonic`/`axum`/`tower-http` gRPC stack for
  embedded-replica sync — a feature nothing in this codebase calls
  (verified: only `Builder::new_local`/`new_remote` are used, both
  covered by the much lighter `remote`/`core`/`tls` features). Trimmed
  accordingly: 504 → 468 transitive deps (-36 crates, tonic and axum
  now fully absent from `Cargo.lock`; incidentally also dropped 3
  windows-* crates that were only pulled in by the gRPC stack, despite
  Windows not being a supported target). Target is still ≤300; the
  remaining ~168 are mostly legitimate (Cranelift, Wasmtime, PyO3,
  libsql-core, tokio, tower-lsp) or ordinary cross-ecosystem version
  skew (34 duplicate package names at different major versions pinned
  by unrelated upstream crates — not fixable without replacing those
  upstream deps entirely, a much larger and riskier undertaking for
  marginal benefit).
- **Bullets 4 (runtime god-object) and 5 (Windows support) — partially
  landed, remainder scoped and deferred.** `src/runtime/mod.rs` started
  this session at 6447 lines (a real god-object); three same-day
  extractions (VM callback bridges, agent tool-calling, LLM dispatch — see
  bullet 4 below) cut it to 4314 lines (-33%). The remaining ~4000-line
  core scheduling/actor-stepping block is a materially higher-risk,
  multi-day refactor entangled with the `unsafe` ORCA GC and cross-shard
  concurrency invariants AGENTS.md flags as "do not break" — deferred as a
  unit, not rushed. A same-day (2026-08-03) follow-up spot-check of
  `recover_actor` as a possible narrower first cut found it shares the same
  hot-path primitives plus an unenforced `current_actor` reentrancy
  assumption, confirming no lower-risk subset exists (full write-up under
  bullet 4). Windows support confirmed at effectively 0% (2 mentions of
  "windows" in all of `src/`) — needs a transport-layer port, path-handling
  audit, and a second CI runner at minimum; a multi-week effort, not
  started.

**Goal.** The language withstands adversarial correctness review. The
runtime withstands adversarial operational review. Both hold up as
"actually production-grade" not "compiled and ran."

**Deliverables.**

1. **Formal semantics completion.** Prove the theorems that already have
   definitions in `spec/formal/`:
   - `types.lean`: `progress`/`preservation`/`type_soundness` —
     **proved 2026-08-14**.
   - `capabilities.lean`: `cap_sendable` (only `val`/`tag` cross actor
     boundaries) — proved; `linear_iso_at_most_once` — open, needs the
     split-context `HasTypeCap` refinement.
   - `effects.lean`: `effect_safety` (closed row `{}` cannot perform an
     unhandled effect) — still a `True` stub, not proved; progress+
     preservation for handler dispatch — open.
   - `combined.lean`: type + capability + effect judgment soundness —
     open.
   - CI gate on `lake build` blocks any PR that touches
     `src/typechecker.rs`, `src/effect_checker.rs`, or `src/types.rs`
     without a corresponding Lean update or an explicit `@sorry_ok`
     annotation reviewed by the steward.
2. **LinearIso must-use enforcement.** Upgrade the at-most-once check
   in `CapabilityAnalyzer` (`src/effect_checker.rs`) to exactly-once
   with a proof. The Lean statement is the source of truth.
   **Partial progress (2026-08-02):** exactly-once is now enforced for
   `let`-bound linear values (`Expr::Let`'s must-use check, with a
   transparent-rebind exemption for bare `let a = x` aliases — 8 new
   tests). Still open: function/lambda parameter-level must-use (a
   linear value already bound in the *initial* context, e.g. a
   parameter, is not yet checked), and the Lean proof itself
   (`linear_at_most_once` in `capabilities.lean` is still `sorry` —
   the Rust-side implementation moved ahead of the formal statement,
   and the statement requires the split-context refinement of
   `HasTypeCap`, documented 2026-08-14).
3. **Backend-trait completion (RFC 0003 item 6 full wiring).** Route
   `src/jit/`, `src/mir_wasm.rs`, `src/wasm_runtime.rs`, and
   `src/python/` behind the traits already defined in `src/backends/`.
   Core language crate imports zero of `cranelift`, `wasmtime`,
   `pyo3`, `libsql`, `quinn`, `rustls`, `reqwest`. Enforced by
   `verify_implementation.py`.
4. **Runtime god-object completion (RFC 0003 item 10 full).** Extract
   `Scheduler`, `GcCoordinator`, `SupervisorTree`, `PersistenceLayer`,
   and `Cluster` from `src/runtime/mod.rs` into standalone structs
   owned by `Runtime`. Each behind its own trait. Enables independent
   evolution and independent test harnesses.
   **Partial progress (2026-08-02):** `mod.rs` was 6447 lines at
   session start; three extractions (free functions taking `&Runtime`/
   `&mut Runtime`, following the pattern already established by
   `workflow.rs`/`exit.rs`/`distribution.rs`/`spawn.rs`/`agent.rs`,
   not yet the full standalone-struct-behind-a-trait vision this
   bullet describes) brought it to 4314 lines (-33%): VM callback
   bridges into `callbacks.rs` (-1528 lines, a verbatim cut of one
   contiguous, self-contained block), agent tool-calling into
   `agent.rs` (-284), and LLM dispatch/retry/suspend into `llm.rs`
   (-381, including the most delicate function moved so far —
   `resume_suspended_llm_step`'s raw-pointer VM-callback reinstallation,
   verified not to disturb the `vm_exec_begin`/`vm_exec_end`
   receive-wait-wake deferral invariant AGENTS.md documents). Each
   extraction verified via clean `cargo check` on both default and
   `--no-default-features`, full lib test suite (1576/1578, unchanged
   baseline), 239/239 conformance, `cargo fmt`, and clippy warning
   count parity (191, full workspace, before/after every commit).
   **Deliberately not attempted this session:** the remaining
   ~4000-line `impl Runtime` block is dominated by core scheduling/
   actor-stepping methods (`step_actor` at 399 lines, `recover_actor`,
   `ask_actor_sync_inner`) deeply entangled with the GC/concurrency
   invariants AGENTS.md flags as "do not break" (the reclamation
   protocol, `vm_execution_depth` tracking) — a materially higher risk
   profile than the AI/LLM subsystem extracted here, and better suited
   to a dedicated, fresh session than squeezed in at the end of a long
   one. The full trait-based structural decomposition this bullet
   originally envisions remains entirely open.
   **Confirmed: no lower-risk sub-target exists in the deferred block
   (2026-08-03).** `recover_actor` was the best candidate for a narrower
   first cut — it reads as cold-start/recovery code, not the scheduler hot
   path — but a full read shows it shares the exact same hot-path
   primitives and an unenforced invariant. Its workflow-resume branch calls
   `send_message_by_id` directly (`mod.rs:3643`); its journal-replay branch
   calls `run_bytecode_behavior` (`mod.rs:3667-3674`), the identical
   primitive `step_actor` and `flush_actor_mailbox` call. Its
   `current_actor` bracketing around that call (`mod.rs:3668,3670`:
   unconditional `Some(actor_id)` in, hard `None` out) matches
   `step_actor`'s own top-level-only pattern (`mod.rs:2148`) — not the
   save-`prev`/restore-`prev` pattern `flush_actor_mailbox` uses for the
   *same* `run_bytecode_behavior` call (`mod.rs:1504-1508`). Two call sites
   into one primitive, two different reentrancy assumptions, reconciled
   only by `recover_actor` always running before the recovered actor is
   enqueued — so `current_actor` happens to already be `None`. That holds
   by call-site discipline alone: nothing in the type system or the test
   suite enforces it, and none of `recover_actor`'s ~20 direct call sites
   (`stress_tests.rs`, `integration_tests/mod.rs`, `runtime/tests.rs`)
   exercise it from inside another actor's live context, unlike
   `step_actor`, implicitly covered by nearly every test that calls
   `run_scheduler()`. A mechanical extraction that normalized the two
   `current_actor` patterns to match — plausible cleanup, wrong in either
   direction — would land with zero failing tests. If this block is
   revisited: unify both call sites behind one audited helper (e.g.
   `with_current_actor(id, ...)`) that owns save/restore, before attempting
   any extraction, so there is one reentrancy contract instead of two
   undocumented ones. Until then, no subset of the remaining block is
   lower-risk than the whole; it stays deferred as a unit.
5. **Windows support.** Port build.rs (currently Fedora-specific
   Python symlink), test the mimalloc + Cranelift path, verify JIT
   symbol linking on MSVC. Add `windows-latest` to the CI matrix.
6. **Release binaries.** GitHub Releases workflow (`release.yml` is
   already scaffolded) produces:
   - `nulang-linux-x86_64`, `nulang-linux-aarch64`.
   - `nulang-macos-x86_64`, `nulang-macos-aarch64`.
   - `nulang-windows-x86_64`.
   Each binary passes the full conformance suite on its target
   platform. SHA-256 sums signed with a project key.
7. **Language server hardening.** Every LSP feature has integration
   tests via `tower-lsp`'s test harness. `cargo run -- --lsp` runs
   for 24 hours against a large `.nula` corpus without leaking
   memory (checked with `heaptrack`).
8. **Dep audit and reduction.** 472 transitive deps (verified `Cargo.lock`
   `grep -c '^name = '` = 472, 2026-08-14; the previously-listed trim
   candidates — `httparse` + `ureq` (unify), `rustyline`'s feature surface,
   `tracing-subscriber` heavy features — remain untrimmed, rationale
   unchanged) → target ≤300.
   `libsql` itself is now feature-trimmed to `core`+`remote`+`tls`
   (dropped `replication`/`sync`, -36 crates including all of tonic/
   axum) but its `core`/FFI/bindgen layer remains — full replacement
   with a bytecode-only journal format is still a candidate if further
   reduction is needed. Every dep gets a "why we depend on this" line
   in `SPEC2.md` §Implementation Status.

**Acceptance.**
- All Frozen and Stable theorems proved in Lean, 0 sorries.
- Backend traits fully wired; core crate deps audit-clean.
- Windows CI green.
- Release v0.3.0 tagged with signed binaries for 5 targets.

**Non-goals.** Bootstrap self-hosting (Phase 3). Package registry
(Phase 3).

**Delegable to.** Formal methods contributor (bullets 1, 2). Rust
platform engineer (bullets 3, 4, 5). Release engineering (bullet 6).
LSP maintainer (bullet 7). Steward retains bullet 8.

**Kill criteria.** If the Lean proofs surface a soundness bug in the
current implementation, freeze the language version and issue a patch
release before continuing. If Windows support turns out to need >4
weeks (Cranelift/PyO3 quirks), split Windows into its own phase.

---

## Phase 3 — Longevity Foundation (weeks 24–52)

**Goal.** Nulang's 200-year story is defensible. The frozen formats have
a self-hosting compiler that emits them. There is a path to a second
implementation. Content-addressed dependencies actually work.

**Deliverables.**

1. **Bootstrap self-hosting.** Advance `bootstrap/compiler_core.nula`
   from Stage 13 to self-compilation. Milestones:
   - Stage 14: module-level parsing (multiple `fn` definitions).
   - Stage 15: multi-binding closure capture via `CapStore`/`CapLoad`.
   - Stage 16: HM inference sufficient for the compiler's own source.
   - Stage 17: type ascription syntax.
   - Stage 18: `compiler_core.nula` compiles itself; byte-identical
     output from stage-N+1 and stage-N+2 (fixpoint reached).
   - Verified in CI: `nulang bootstrap/compiler_core.nula < bootstrap/self.nula`
     produces `.nbc` byte-identical to `cargo run -- bootstrap/self.nula`.
2. **Package registry.** **[X] — closed 2026-08-14 (landed 2026-08-05, `ce20407`).** Minimum-viable, boring, static-file registry:
   - Host `.nbc` artifacts + `Nulang.toml` manifests on a git-backed
     store (GitHub Pages or Cloudflare R2).
   - Content-addressed by BLAKE3 (RFC 0003 item 11 already ships the
     lockfile hashing).
   - `nula publish`, `nula add <name>` (no path/git required).
   - Namespace ownership by TXT-record verification, transferrable.
   - Rate limits and moderation on the registry index only; content
     is immutable and CDN-cacheable.
3. **Second implementation seed.** The bootstrap compiler *is* the
   second implementation for the Core fragment. Beyond that, publish
   a Written Rules of Engagement for a second implementation: which
   parts of `SPEC2.md` are non-negotiable, which are hints, how a
   competing implementation registers as conforming (passes
   `conformance/`).
4. **RFC 0010 keyword audit follow-through.** ✅ **closed 2026-08-14.** Executed per RFC 0010 §C.6: `where`, `priv`, `loop`, `node`, `subworkflow` freed as identifiers (pinned by `test_former_keywords_now_identifiers`, `src/lexer.rs:1538`); `monitor`/`link`/`exit` wired as live syntax (`spawn link|monitor` modifiers, `src/parser.rs:3601-3605`; `perform Actor.link/monitor/exit` op names, `:3806-3814`); `await` re-reserved by design (`lexer.rs:1196`). Keyword lifecycle documented in GOVERNANCE §2a; SPEC2 §2.3 inventory synced to the lexer (see sweep).
5. **Escape analysis or region inference.** Reintroduce
   `src/escape_analysis.rs` (the earlier version was reverted, see
   `PERFORMANCE_ANALYSIS.md` row 2.4). Goal: statically prove
   stack-allocation for containers that never leave a function. Wire
   into the JIT tier so hot loops with local records/arrays never hit
   the heap. Measure via the Phase 1 bench dashboard.
6. **CRDT op-based replication (CmRDT).** **[X] — closed 2026-08-14 (landed via Phase 5 D13).** Delta-state ships in 1.0.0;
   op-based is the missing complement per `PERFORMANCE_ANALYSIS.md`
   row 3.2. Ship `Packet::CrdtOp` alongside `CrdtDeltaSync`. Provides
   the lowest-bandwidth sync path.
**Pulled forward into Phase 5** (Distributed Systems Excellence,
deliverable 13) — executed there, not gated on Phase 3's own timeline;
this bullet is satisfied by reference once Phase 5's CmRDT deliverable
lands.
7. **Deprecation cycle graduations.** Per `GOVERNANCE.md` §6, the
   deprecated surfaces from 1.0.0-frozen (LLM effect, `LlmAsk` opcode,
   `Pipeline`/`Supervisor`/`Debate` in-language modules) either move
   out of the language surface into `nulang-ai` stdlib or graduate
   to real removal. Requires bytecode v1→v2 migration in
   `src/format/migrate.rs`.

**Acceptance.**
- Bootstrap compiler passes its own byte-identity test.
- Registry live; 5 packages published by non-steward authors.
- Second-implementation ROE published.
- Language version bump: 2.0.0-frozen (if migrations were required)
  or 1.1.0-stable.

**Non-goals.** Wide adoption push (Phase 4). New backends. Perf work
outside escape analysis.

**Delegable to.** Language implementer for bootstrap (multi-month
effort by one specialist). Ops/infra for registry. Steward retains
bullets 3, 4, 7.

**Kill criteria.** If self-hosting surfaces a fundamental gap in Core
(e.g. it needs features currently outside Core), that gap is an RFC to
extend Core, gated by the steward, not a workaround in the bootstrap.
If it takes >6 months, ship what works and defer self-compilation to
Phase 4.

---

## Phase 4 — Ecosystem and Adoption (weeks 24–52+, parallel with Phase 3)

**Goal.** Nulang has users the maintainers don't personally know. It
has a killer application demonstrating a category it wins.

**Deliverables.**

1. **Reference application.** One production-quality application in
   `examples/` or a sibling repo that demonstrates what Nulang does
   better than anything else. Candidate: a distributed, durable,
   supervised AI-agent orchestrator (leverages `entity`, `workflow`,
   `Inference.ask`, supervision, CRDTs, persistence — all the parts
   no other language has together). Alternatives:
   - A distributed KV with per-key CRDT choice.
   - A fault-tolerant IoT ingester with location-transparent routing.
   Chosen application ships as a runnable demo, a blog post, and a
   `nula run` one-liner.
2. **Documentation completeness.**
   - `docs/TUTORIAL.md` verified end-to-end by CI.
   - `docs/PITFALLS.md` extended from lessons learned in Phases 0-2.
   - Book-length treatment (`docs/book/`) covering: type system,
     effects, capabilities, actor model, distribution, persistence,
     AI runtime, WASM, FFI. Deliverable: `mdBook` output published
     to `docs.nulang.org`.
   - Migration guides: "coming from Erlang", "coming from Rust",
     "coming from Elixir". Each with a translated non-trivial
     example.
3. **First external user.** Actively pursue one. This is a
   relationship-building task, not a technical task, and the steward
   owns it. Success looks like: an outside team runs Nulang in
   production (broadly defined — even internal tools count) and files
   at least one bug the steward didn't already know about.
4. **Community infrastructure.**
   - Discord/Zulip/Matrix (one, not three).
   - Weekly office hours for the first 3 months.
   - Public roadmap on GitHub Projects mirroring this file.
   - Contribution guide with the RFC process front-loaded.
   - Code of conduct.
5. **VS Code extension published to marketplace.** The
   `.vscode/extension.js` scaffold exists; publish it. Same for a
   Zed extension and a Neovim plugin.
6. **`nula` template library.** ✅ **Grew to 7 templates 2026-08-09**
   (`distributed`, `ai-agent`, `web` added on top of `default`/`cli`/
   `lib`/`full`). `--template distributed` spawns two message-passing
   worker actors; `--template ai-agent` is an actor backed by
   `perform Inference.ask`; `--template web` is an HTTP client via
   `Http.get`/`Http.post`. The PLAN's original "web (HTTP server +
   JSON)" framing is partially deferred: the server template uses the
   `Http.serve` effect, which is currently only wired through the
   integration-test harness and returns "Unhandled effect" via the CLI
   (pre-existing gap, tracked for follow-up) — so the shipped web
   template uses the client effects that work via the CLI. Each new
   template was validated end-to-end (`nula new` → `nula run`); 2 new
   tests cover all 7 templates scaffolding a valid entry point and
   unknown-template rejection.
   **Follow-up resolved 2026-08-09 (`a6b0172`):** the standalone-VM
   `Http.serve` dispatch gap is fixed — `StandaloneVmCallbacks` now
   handles `Http.serve` (was "Unhandled effect"), proven by
   `test_http_serve_standalone` making a real request. The web template
   nonetheless stays an HTTP **client** (`Http.get`/`Http.post`): a pure
   standalone `Http.serve` program exits when `main` returns (the leaked
   listener thread dies with the process), so a server template needs a
   blocking program shape that doesn't fit the one-shot `nula run` model.
   A runtime-backed or blocking program can use `Http.serve` directly.
7. **Speaking + writing.** One conference talk (Strange Loop, Papers
   We Love, LambdaConf), one long-form technical post per quarter
   (JIT internals, capability system, DST harness, formal semantics).

**Acceptance.**
- One non-steward production user (with permission to name them).
- Reference app: 1000+ GitHub stars or equivalent traction signal.
- Book published; tutorial verified.
- 3+ merged PRs from non-steward contributors.

**Non-goals.** Chasing hype. Framework proliferation. Adding surface
to appear more-featured.

**Delegable to.** Docs writer (bullet 2). DevRel-shaped contributor
(bullets 4, 7). Steward owns bullets 1, 3, 5, 6 initially.

**Kill criteria.** If after Phase 3 the reference application has no
traction, re-evaluate the pitch. Some 200-year languages find their
niche late; that is fine, but requires honesty about the current pitch
not landing.

---

## Phase 5 — Distributed Systems Excellence (parallel with Phases 1-3)

**Status (2026-08-15): all 18 deliverables implemented. D7c (RFC 0014,
durable-actor re-spawn on node failure) landed 2026-08-15, closing the last
Phase 5 open item.**

**Goal.** The distributed actor runtime — not just the single-node
language — withstands adversarial operational review. A cluster survives a
real network partition without silent data loss or a stuck split-brain;
the default transport is authenticated and encrypted; a node's death
triggers real recovery instead of silent orphaning; CRDT state doesn't
leak memory forever; an operator can point off-the-shelf tooling at a
running cluster. This is where Nulang's distributed-actor design either
becomes provably best-in-class or stays a credible-looking demo.

**Sequencing.** All Groups A-G are implemented (D1-D18); no Phase 5 items
remain open.

**Deliverables.**

1. **Split-brain resolver.** ✅ **landed 2026-08-03..13 (RFC 0011).**
   `SplitBrainResolver` trait + `static-quorum`
   (`cluster.rs:250-254`), down-self + probe re-join, commits `5a0b641`
   (partition injection + virtual-clock chaos) and `1498cc7`
   (static-quorum cold-bootstrap guard + e2e down-self test).
   `keep-majority`/`keep-oldest` remain deferred (live-count accuracy not
   yet proven). RFC required (Frozen/Stable cluster-membership
   tiering) — satisfied by RFC 0011.
2. **DST-driven split-brain and asymmetric-partition test coverage.**
   ✅ **Landed 2026-08-03** as `src/runtime/cluster_sim.rs` (test-gated
   `SimCluster`, wired in `runtime/mod.rs`): N real `ClusterState`
   machines against a shared `VirtualClock` advanced in lockstep, with a
   directed cut-able message fabric (heartbeats, gossip, and probes —
   probes delivered as the heartbeat packets they are on the wire;
   deliveries to a downed node dropped, mirroring its shut-down
   transport). Five deterministic scenarios: clean 2/3 partition of a
   5-node cluster (minority downs itself at the 2 s Suspicious mark —
   the resolver counts only Healthy/Joining as reachable — majority
   survives and keeps the minority Failed after healing; downed nodes
   stay down, operator restart is the recovery path), asymmetric
   one-way partition (three phases: the 2-4 s asymmetry window where the
   silent node is down but still Healthy on the other side, mutual
   Failed at 10 s, heal-without-recovery), probe-based re-join of a
   healed 2/3 partition under quorum 2 (no downing; full mesh
   convergence via probes, no external rejoin), the documented
   2-node fail-closed caveat, and a seed-driven 50-partition invariant
   sweep (no node downs itself while it still sees quorum). This is the
   verification vehicle for deliverable 1 — the real-TCP chaos tests
   (`tests.rs`) stay as-is; the deterministic suite is what can
   eventually scale to many-seeds CI runs.
   **Deliverable 3 (three doc-vs-code bugs) was already fixed by the
   deliverable-1 work**: `pick_gossip_targets` is now OsRng-driven
   partial Fisher-Yates (not deterministic first-N), the dead
   `TcpTransport` `next_seq` `AtomicU64` is gone (sender-local counter
   only), and `NodeInfo`'s vestigial standalone `incarnation` field was
   removed — only the wire `NodeGossip.incarnation` remains.
3. **Fix three confirmed doc-vs-code / dead-code bugs found this session**
   (cheap, do first as warm-up, independent of 1-2): (a)
   `cluster.rs:460`'s comment "Gossip to a random subset of healthy nodes"
   and `cluster.rs:24-25`'s module doc both claim randomness;
   `pick_gossip_targets` (`cluster.rs:652-663`) actually does "simple
   deterministic selection: pick the first N" and says so in its own
   comment ("in a real deployment this would use
   `rand::seq::IteratorRandom`") — wire in real random selection
   (deterministic first-N systematically starves whichever members sort
   late in `HashMap` iteration order from gossip coverage); (b)
   `network.rs`'s `TcpTransport.next_seq` `AtomicU64` is incremented in
   `send()` but discarded (`let _ = seq;`, `network.rs:1472-1478`) while
   the sender thread keeps a second, actually-used local sequence counter
   (`network.rs:1742-1745`) — two divergent counters; delete the dead one;
   (c) `cluster.rs`'s standalone `incarnation` field
   (`cluster.rs:221-223`, bumped by `bump_incarnation` `:538-540`) is
   never transmitted on the wire — only the separate per-entry
   `_incarnation` metadata string is (the one `merge_membership`/AGENTS.md
   actually document and test) — delete the vestigial field.
4. **Wire up authenticated, encrypted transport, with plaintext as an
   explicit opt-out.** ✅ **landed 2026-08-04/05 (RFC 0013, Implemented).**
   `TlsConfig::{MutualTls, PlaintextInsecure}` (`network.rs`), node
   identity from cert fingerprint via BLAKE3, real CA verification,
   commits `0ab2c42` + `59b01bd` (SelfSigned removed, rcgen →
   dev-dependencies). The wire handshake stayed additive over NUL0 v1 (no
   version bump).
5. **Decide QUIC's fate — finish or remove, not permanent dead weight.**
   ✅ **Removed 2026-08-05.** Assessed for integration: requires a tokio
   runtime (separate from the main sync runtime), has an incompatible raw
   8-byte handshake with no NUL0 magic/version check, and zero test
   coverage. The TCP transport with MutualTLS (deliverable 4) already
   provides authenticated, encrypted transport. Removed
   `src/runtime/quic_transport.rs`, `quic-experimental` feature, and
   `quinn` dependency. `rcgen` is preserved (used by
   `TlsConfig::SelfSigned`). QUIC can be revisited when users ask for
   multiplexed transport; not needed for alpha.
6. **Partial-view membership beyond full-mesh.**
   ✅ **Landed 2026-08-03** — the heartbeat data plane is now
   O(active view) instead of O(every member), with the membership table
   and gossip unchanged (no wire change; views are local state, same
   Experimental tier as RFC 0011, which this work amends with §6):
   - **Active view (4) / passive view (20) / probation**: admission by
     incoming heartbeat (reciprocity evidence); the failure detector
     watches exactly the active view, so no member we do not heartbeat
     can be false-failed. A failed active member is repaired by
     promoting a Healthy passive to probation (heartbeated, not
     watched); first reply confirms, silence demotes (churn, not false
     failure), retry every 5 s.
   - **Bounded reply rule**: up to `REPLY_SLOTS` (4) replies per round
     to recent passive pingers (rotated) — a member whose view filled
     up still gets answered within the 2 s detection window (~80-node
     ceiling at these constants).
   - **Detector bumps incarnation on `Failed`** so the status
     propagates via gossip to non-watchers (invisible under full-mesh,
     fatal under partial view — this was a real gap found by the DST
     harness).
   - **Gossip liveness refresh** for passive live members
     (equal-incarnation re-broadcast refreshes `last_heartbeat`;
     watched members and Failed entries are never refreshed — the
     dead-peer protection regression-tested).
   - **Freshness-aware resolver view**: stale-status passives count as
     Suspicious in the view handed to the resolver, so an isolated
     node's frozen gossip cannot keep it above quorum; `static-quorum`
     stays correct under partial view. `keep-majority`/`keep-oldest`
     remain deferred (live-count accuracy not yet proven — unchanged
     from the RFC).
   - **Verification**: 4 new `SimCluster` scenarios (30-node bounded
     fanout, 10-node convergence, death with zero false failures +
     gossip failure propagation, heal/rejoin with view repair) + 6 unit
     tests for the view mechanics. The DST harness is what surfaced
     both real gaps (incarnation bump, stale-gossip quorum) — the
     plan's "verification vehicle" reasoning paid off.
7. **Node-death detection triggers real recovery, not silent orphaning.**
   ✅ **Landed 2026-08-09..15 (RFC 0014).** Parts (a)+(b) (`handle_node_failed`
   in `distribution.rs`, wired to `ClusterAction::NodeFailed`): (a) the dead
   node's `RemoteActorCache` entries are invalidated so sends fail fast
   instead of stale-resolving; (b) every local actor that had linked or
   monitored an actor on the failed node receives a
   `DOWN`-with-`noconnection` system message (new `ExitReason::NoConnection`,
   payload code 6) and the dead registry entries are dropped. The D8
   delivery half also landed: inbound `Packet::Link`/`Monitor` now register
   remote watchers and inbound `Packet::Down` delivers DOWN to local
   watchers. Part (c) — the recovery half — landed 2026-08-15 per
   **RFC 0014**: a confirmed-gone membership state (promoted from `Failed`
   past `removal_confirmation_timeout` under quorum, or immediately on a
   positive `Packet::NodeGoodbye`), a gossip-replicated durable-actor
   location directory (`DurableDirectoryEntry`, highest-epoch-wins),
   shadow-node snapshot replication at `checkpoint_actor` (via the new
   `Packet::ShadowReplicate`, re-spawned through the existing
   `receive_migrated_actor`), a `RestartPolicy::RespawnOnNodeLoss`
   supervisor policy (`.nula` `Otp.supervise_child` policy `3`), and
   epoch-based two-live-copies resolution (`self_demote_superseded` on
   re-join). The goodbye path checkpoints+terminates the opted actors before
   declaring them dead, so a re-spawn never races a still-live copy.
   Verified by directory/removal/goodbye unit tests, a deterministic
   `cluster_dst` kill→re-spawn scenario (two checkpoints, latest wins), a
   goodbye-path duplicate assertion, a self-demote test, and an end-to-end
   `.nula` policy-3 opt-in test.
   **Original analysis (kept for provenance):** zero grep hits for
   `failover`/`rehome`/`migrat` logic across
   `distribution.rs`/`distributed.rs`. `ClusterState` already detects
   `Failed` nodes via a staged 2s/5s/60s timeout (`cluster.rs:381-433`) —
   that signal today goes nowhere except membership bookkeeping. When a
   node transitions to `Failed`: (a) invalidate that node's entries in
   every other node's `RemoteActorCache` so sends fail fast instead of
   stale-resolving: (b) fire link/monitor-equivalent
   `DOWN`-with-`noconnection` notifications to local actors that had
   linked/monitored an actor known to live on the failed node — this
   requires deliverable 8 to exist first, since today link/monitor tables
   have no remote-actor entries to notify; (c) for actors backed by
   durable state, enable an explicit supervisor-policy-driven re-spawn on
   a healthy node from the last durable snapshot. Do NOT implement silent
   automatic migration on node failure — without an explicit supervisor
   policy confirming the old node is actually gone (not just partitioned),
   automatic re-spawn risks two live copies of the same durable-id actor
   writing to the same store from two nodes. Model the safety gate on
   Kubernetes StatefulSet pod rescheduling requiring
   old-pod-confirmed-gone, not naive auto-failover.
8. **Cross-node link/monitor registration.** ✅ **landed 2026-08-04
   (RFC 0012).** `Packet::Link`/`Monitor`/`Down` wire types,
   `RemoteLinkRegistry`/`RemoteMonitorRegistry`, commit `0ab2c42`.
9. **Fix the nested-supervisor-restart bug.** ✅ **landed 2026-08-03
   (`22a56c7`).** `rebuild_child` recreates the `Supervisor` struct under
   the new actor id (`supervisor.rs:396-422`); regression test
   `test_supervised_supervisor_keeps_supervising_after_restart`
   (`tests.rs:293-346`).
10. **Fix the mass-restart rate-limit bug.** ✅ **landed 2026-08-03
    (`22a56c7`).** Per-sibling `should_restart` guards in `restart_all`
    (`supervisor.rs:533`) and `restart_from` (`:575`); regression tests
    `test_one_for_all_respects_sibling_rate_limit` (`tests.rs:348-399`) and
    `test_rest_for_one_respects_sibling_rate_limit` (`tests.rs:401-450`).
11. **Tombstone garbage collection for `ORSet`/`AWORSet`/`RGA`.**
    ✅ **landed 2026-08-08 (`492cd72`, causal-stability watermark).**
    `gc_stable_tombstones` (`crdt_manager.rs:789`); Group-C dependency
    satisfied by D6's membership bookkeeping.
12. **Wire `state crdt` into real `.nula`-level syntax.** ✅ **landed
    2026-08-04 (selector) + 2026-08-15 (effect module, enforcement,
    conformance).** The concrete-CRDT-type selector
    (`parser.rs:1752-1809`, `CrdtType::from_keyword`), the `Crdt.*` effect
    module (`perform Crdt.increment/decrement/add/remove/set/read`, stdlib
    registry + `perform_crdt_builtin` in `runtime/mod.rs`), per-type
    operation-set enforcement (`CrdtManager::apply_field_op` rejects
    out-of-set ops; raw `self.field = expr` assignment to a crdt field is
    ignored in both callback `set_state_field` impls), and `.nula`-level
    conformance cases (`conformance/behavior/crdt_gcounter.nula`,
    `crdt_pncounter.nula`, `crdt_gcounter_opset.nula`). The standalone
    runtime now initializes `crdt_manager` eagerly, so `state crdt` fields
    register and `Crdt.*` works without distribution enabled. **Known gap
    (docs-truthed 2026-08-15):** `recover_actor` does not rebuild
    `CrdtManager.field_map`, so `Crdt.*` is a silent nil no-op on a
    recovered actor (the materialized `state_data` value survives) — pinned
    by `test_crdt_field_survives_recovery`.
13. **Op-based CRDT replication (CmRDT).** ✅ **landed (Phase 3 bullet 6
    satisfied by reference).** `Packet::CrdtOp` (`network.rs:595`),
    `CrdtManager::apply_op` (`crdt_manager.rs:511`).
14. **Real actor migration, not a no-op stub.** ✅ **landed 2026-08-10
    (`1950c01`).** `DistributedVmCallbacks::migrate` has a real body
    (`callbacks.rs:1593`: snapshot + nbc extraction, `reap_living_actor`);
    `OpCode::Migrate` drains (`vm.rs:4672-4674`); forwarding via
    `migrated_actors` entries (`mod.rs:1786`). Reentrancy caution below
    remains load-bearing for the future D7c work: `recover_actor` has an
    unconditional-`Some`/hard-`None` `current_actor` bracketing pattern
    around its `run_bytecode_behavior` call that differs from
    `flush_actor_mailbox`'s save-`prev`/restore-`prev` pattern for the
    identical primitive — any future spawn-from-snapshot path must match
    whichever is actually reentrancy-safe, not invent a third.
15. **Cross-node durable-store replication: explicitly scoped down, not
    silently deferred.** ✅ **landed 2026-08-03 (`42d879d`).**
    `PRAGMA journal_mode=WAL` (`persistence.rs:811`), operator-configurable
    `PRAGMA synchronous={OFF|NORMAL|FULL}` (`:825`, `SqliteSyncMode`),
    `JsonFileStore` fsyncs journal (`:581`) and workflow (`:609`) appends.
    Full multi-node durable-store replication remains explicitly OUT
    (Raft, `PERFORMANCE_ANALYSIS.md` row 3.4 — deferral unchanged).
    **Residual closed 2026-08-15:** EventSourced `append_event`
    (`persistence.rs:624`) now `sync_all()`s like the journal and workflow
    appends — a lost event append is a lost EventSourced commit.
16. **Fix `LibsqlStore` silently dropping `crdt_snapshot` on save/load.**
    ✅ **landed 2026-08-03 (`42d879d`).** `crdt_snapshot` column + migration
    (`persistence.rs:849/859-861`), save (`:969-976`), load (`:986-1006`);
    tests `test_libsql_store_crdt_snapshot_roundtrip` (`tests.rs:1878-1909`),
    `test_libsql_store_migrates_crdt_snapshot_column` (`tests.rs:1911-1952`).
17. **Ship the distributed-actor-relevant slice of the
    metrics/tracing/debug story SPEC2.md §15.3-15.4 already speculatively
    designs — not the whole chapter.** ✅ **landed 2026-08-08..13
    (`5d15857`, `7592b72`).** (a) `otel` cargo feature +
    `init_tracing` (`src/observability.rs`, `main.rs:61-77`) and a
    `--metrics-port` Prometheus-format server (`main.rs:804/1727`,
    `Runtime::enable_metrics_server`; exports `GcStats`/`SchedulerStats`/
    `ResolverStats`/mailbox depths — `metrics.rs:141-187`); (b)
    `trace_id: Option<String>` riding `Packet::ActorMessage`'s string
    table (`network.rs:547-549/829`); (c) `perform Debug.inspect(actor_id)`
    builtin returning `{ state, mailbox_size, behaviors, supervisor }`
    (`callbacks.rs:438-455` runtime, `vm.rs:664` standalone print form).
    Out of scope, unchanged: `.nula` `metrics.counter`/`trace.span`/
    `config trace` effects remain backlog per the original scope line.
18. **Visual actor topology dashboard.** ✅ **landed 2026-08-15.**
   `deploy/observability/` ships the off-the-shelf-Grafana demo the
   original scope line called for — no bespoke backend:
   `docker-compose.yml` (Prometheus + Grafana with anonymous admin and
   provisioning), `prometheus/prometheus.yml` (scrapes the node's
   `--metrics-port` `/metrics` endpoint via `host.docker.internal`),
   a provisioned Grafana datasource + dashboard provider, and a
   default `nulang-runtime.json` dashboard (9 panels covering live
   actors/DLQ/mailbox gauges and scheduler/GC/resolver counters). All
   20 dashboard metric names verified to match `metrics.rs`
   `to_prometheus_text` exactly; JSON + YAML validated.

**Acceptance.**

- ✅ Default new clusters require authenticated, encrypted transport;
  plaintext is an explicit, documented opt-out, never the silent default
  (deliverable 4 — RFC 0013 + D6).
- ✅ A 3-node and a 5-node DST/chaos scenario suite includes split-brain
  (mutually-invisible healthy sub-clusters) and asymmetric-partition
  cases; the cluster provably converges to one surviving side per the
  configured resolver strategy, never a stuck two-sided split
  (deliverables 1-2 — D1/D2).
- ✅ A node killed mid-run triggers `DOWN` notifications to every local
  actor that had linked/monitored one of its actors (deliverables 7-8 —
  D7a+b + D8). The supervisor-policy-driven re-spawn half remains open
  (D7c, RFC 0014).
- ✅ `ORSet`/`AWORSet`/`RGA` tombstones are garbage-collected once causally
  stable; a long-running soak test shows bounded, not unbounded, memory
  growth (deliverable 11 — D11).
- ✅ `state crdt` fields have real `.nula`-level syntax, a concrete-type
  selector, and merge-on-sync (deliverable 12 — D12 partial; the
  `Crdt.*` effect module and operation-set enforcement remain open).
- ✅ A `migrate`-triggered move relocates a persistent actor to a healthy
  node; the old node's `AddressResolver` cache no longer resolves the old
  location afterward (deliverable 14 — D14).
- ✅ `LibsqlStore` uses WAL plus an explicit `synchronous` pragma and
  round-trips `crdt_snapshot` correctly; `JsonFileStore` fsyncs
  journal/workflow appends with the same discipline as its snapshot path
  (deliverables 15-16 — D15/D16).
- ✅ A running cluster's `GcStats`/`SchedulerStats`/`ResolverStats`/mailbox
  depths are scrapeable by an off-the-shelf Prometheus/OTel collector with
  zero code beyond configuration; a trace begun on one node's actor send
  continues on the node that receives it (deliverable 17 — D17).

**Non-goals.** Native Raft/consensus-backed strongly-consistent
replication (`PERFORMANCE_ANALYSIS.md` row 3.4, still deferred — SBR-style
resolvers give partition safety without it; this phase does not relitigate
that call). Kernel-bypass networking (io_uring/RDMA, row 3.3, still
deferred). Content-addressable bytecode (row 3.5, still deferred). The
non-distributed-actor parts of SPEC2.md Chapter 15 (deployment manifests,
the generic `config app` configuration system, serverless deployment
targets) beyond the observability slice in deliverables 17-18. Automatic
silent actor rebalancing without an explicit supervisor policy
(deliverable 14 is operator/supervisor-triggered only, by design — see its
safety note).

**Delegable to.** Distributed-systems specialist (deliverables 1-10:
split-brain, transport security, membership scaling, cross-node fault
tolerance — the deepest and highest-risk work). CRDT/data-structures
specialist (deliverables 11-13). Storage/persistence engineer
(deliverables 15-16). Observability/SRE-tooling contributor (deliverables
17-18). Steward retains RFC authorship or review for every deliverable
that touches Frozen or Stable surface (4, 8, 12, and 1/14 by the more
cautious operational-blast-radius reasoning) per GOVERNANCE.md §3, and
retains deliverable 14 given its direct dependency on this session's
`recover_actor` findings.

**Kill criteria.** If the split-brain resolver (deliverable 1) cannot be
built on `static-quorum` alone because the cluster has no way to agree on
a configured expected size without agreement itself, stop and re-scope as
a Raft adoption instead of routing around it quietly — this would mean
`PERFORMANCE_ANALYSIS.md`'s Raft deferral was wrong, which is itself
significant enough to interrupt the phase and get steward sign-off, not
something to paper over. If NUL0 v2 (deliverable 4) surfaces a
wire-compatibility break that can't cleanly refuse old versions the way
v1's handshake already does (`network.rs:296-323`), treat it as a
Frozen-tier incident per GOVERNANCE.md and get steward sign-off before
shipping — same bar as any other Frozen-tier change. If partial-view
membership (deliverable 6) can't preserve the accuracy the split-brain
resolver's `keep-majority`/`keep-oldest` strategies need, ship deliverable
1 with `static-quorum` only and defer `keep-majority`/`keep-oldest` and
deliverable 6 together, rather than shipping a resolver that silently
miscounts.

---

## Cross-cutting workstreams (continuous)

- **Governance discipline.** Every Frozen/Stable change is an RFC.
  Every RFC has a Lean update if the theorems touch. Every accepted
  RFC has a conformance case.
- **Security.** `cargo audit` runs in CI (already scheduled). Add
  fuzzing corpus artifacts to CI (persist failing seeds). Publish a
  security policy (`SECURITY.md`) with a disclosure email.
- **Dependency governance.** Every new direct dep requires a PR
  comment justifying it against the small-binary principle.
- **Docs stay live.** `scripts/verify_doc_examples.sh` runs on every
  PR; docs drift is a blocker, not a warning.
- **Distributed-transport test discipline.** Every change to `network.rs`,
  `cluster.rs`, `distributed.rs`, or `distributed_context.rs` ships with a
  DST/chaos scenario covering the failure mode it touches (partition,
  split-brain, node death) before merge, once Phase 5 deliverable 2 lands
  the harness — this makes chaos coverage a standing gate, not a one-time
  sweep.

## Risk register

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| Steward bottleneck | High | High | Every phase names delegable work; hire/recruit specialists per phase |
| Formal proofs surface soundness bug in impl | Medium | Very High | Ship as Sev-1 patch; the Frozen tier is precisely for this |
| Bootstrap takes >6 months | Medium | Medium | Ship staged partial results; defer self-compile fixpoint if needed |
| WASM/AOT can't be finished usefully | Medium | Low | Truth-in-advertising phase already downgrades them |
| Package registry becomes moderation nightmare | Medium | Medium | Namespace verification via DNS; immutable content; no editorial voice |
| Cranelift API breakage on Rust upgrade | Low | Medium | Backend trait boundary (Phase 2 bullet 3) isolates the risk |
| No external user materializes | High | Very High | The reference application is the mitigation; if that doesn't land, re-evaluate pitch |
| Feature creep from AI-runtime enthusiasm | Medium | High | Language surface stays actor + effects + capabilities; AI stays in `nulang-ai` |
| Split-brain resolver false-positive downs a healthy majority | Medium | High | Default to the conservative `static-quorum` strategy; `keep-majority`/`keep-oldest` require explicit opt-in once membership-count accuracy is proven under Phase 5 deliverable 6 (mitigated: RFC 0011 static-quorum landed with down-self tests, 2026-08-13) |
| NUL0 v2 transport-security bump breaks embedders relying on today's no-config plaintext `enable_distribution(addr, None)` | Medium | Medium | Plaintext stays available as an explicit, documented opt-out variant, never silently removed; existing `None` call sites migrate to an explicit insecure variant, not a breaking signature change (superseded: RFC 0013 landed additive over NUL0 v1; plaintext is the explicit `PlaintextInsecure` variant) |
| Partial-view membership (Phase 5 deliverable 6) undermines the split-brain resolver's member-count assumptions (deliverable 1) | Medium | Medium | Sequence deliverable 1 before deliverable 6; if 6 can't preserve count accuracy, ship 1 alone and defer 6 (see Phase 5 kill criteria) (mitigated: RFC 0011 static-quorum landed with down-self tests, 2026-08-13) |

## Version + tier progression

| Version | Milestone | Trigger |
|---|---|---|
| 0.1.0 | current alpha | shipped |
| 0.2.0 | Phase 0+1 complete | truth-in-advertising + correctness floor |
| 0.3.0 | Phase 2 complete | proofs + Windows + release binaries |
| 1.1.0-stable | Phase 3 partial | bootstrap fixpoint + registry live |
| 2.0.0-frozen | Phase 3 complete | deprecation cycle graduations require major bump |
| 2.0.0-frozen (or sooner) | Phase 5 NUL0 v2 | authenticated/encrypted transport handshake is a Frozen-tier wire-format bump (deliverable 4) — an independent trigger from Phase 3's deprecation graduations; whichever lands first bumps the major language version, the other rides the same or a later major bump (not needed: RFC 0013 shipped authenticated transport additive over NUL0 v1) |

Phase 5 runs in parallel with Phases 1-3 and is not gated on their
completion; its Frozen/Stable-tier deliverables (1, 4, 8, 12, 14) each
need their own RFC per the governance-discipline workstream above,
independent of the phase's own internal sequencing.

Language version moves only per `GOVERNANCE.md` §5. Crate version
revs freely.

---

## What this plan is not

- Not a wish list. Every item cites the file it modifies or the RFC it
  implements.
- Not a hiring plan. It scales to one steward + rotating specialists.
- Not a fundraise deck. The 200-year framing is a design constraint,
  not a valuation.
- Not immutable. Kill criteria and re-plan triggers are load-bearing.

## What this plan is

An honest sequence of the work between an alpha language with excellent
bones and a language a serious team would trust in production. The
sequencing is: stop lying, start proving, self-host, ship users. Every
phase before the last one is defensive work — the goal is that when the
first external user does show up, nothing they touch is a stub.
