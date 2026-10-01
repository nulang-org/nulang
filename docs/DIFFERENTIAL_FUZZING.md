# Differential Fuzzing: VM ↔ JIT ↔ AOT ↔ WASM

Cross-backend semantic fuzzing for Nulang. Every generated program is
executed by the semantic-reference VM plus each enabled backend that accepts
the program, and observable outcomes must agree exactly; any divergence is a
backend correctness bug.

## Backends compared

1. **Bytecode VM (cold)** — `VM::new_without_jit()`, pure interpreter. The
   interpreter's step limit bounds runaway programs.
2. **VM + forced JIT tier-up (warm)** — `VM::new()`, run repeatedly
   (> `HOT_THRESHOLD` times, capped by a 25 ms warmup budget) so hot
   regions compile to native code, then a final timed run. Programs with
   internal hot loops (trip counts > 1000) also tier up within a single
   run.
3. **AOT native** — `AotModule::compile(&mir).run()`, when the program is
   within AOT's supported subset (effects/actors/FFI are rejected at
   compile time with `Unsupported`, which is a skip, not a divergence).
4. **WASM** — enabled with `--features wasm-backend`. Only explicit
   restricted-profile/unsupported-feature compiler diagnostics are counted as
   unsupported; unexpected compiler failures are divergences.
   Once WASM bytes are emitted, however, validation, instantiation, or the
   absence of the required `nulang_init` export is a hard differential
   failure rather than a skip. Successfully executed WASM values/errors must
   agree with the bytecode reference.

## Oracle

`fuzz::differential_fuzz_one(source)`:

- Compile once (lex → parse → typecheck → HIR → MIR → bytecode). Programs
  that don't compile are skipped (the generator aims for well-formed
  programs, so a compile failure is reported separately — it means the
  generator escaped the grammar or the frontend rejects a legal program).
- Cold and warm runs must produce identical comparison keys. Keys are
  `Value::to_string_repr()` for self-contained tags (nil/unit/bool/int/
  float — floats canonicalize NaN), content-resolved text for strings
  (`VM::string_operand`), and normalized error classes for failures
  (`Step limit exceeded` collapses to a fixed marker since it is a
  resource bound, not semantics). Closures/actor refs have no stable
  cross-run identity and are `Uncomparable` (not counted).
- AOT runs compare against the cold key for self-contained tags only
  (AOT has its own constant pool; string pool indices are not comparable
  across independent compilations).
- WASM runs use the same self-contained comparison rule. Only explicit
  restricted-profile/unsupported-feature compile diagnostics are skips;
  unexpected compiler failures and all post-emission runtime rejection are
  divergences. The WASM-enabled test lane also requires positive WASM
  participation so a regression cannot silently turn every generated program
  into an unsupported case.

## Program generator (`src/difffuzz.rs`)

`generate_program(seed: u64) -> String` is a pure, deterministic function
of the seed (xorshift64; the seed is ORed with a nonzero constant to avoid
the all-zero sink state). Programs are generated from a typed grammar
subset so virtually all of them compile:

- **Ints**: literals weighted toward 48-bit boundary values (±2^47±1),
  `+ - * **`, unary neg, div/mod with provably-nonzero divisors
  (`(e)*2+1` is always odd; `if d == 0 then 1 else d` guards).
- **Floats**: ±0.0, decimals, `**` with small exponents, guarded division
  (float div-by-zero yields nil and would poison surrounding arithmetic
  with a type error — valid to compare, but kept rare so most programs
  exercise value semantics rather than error paths).
- **Strings**: literals, `+` concat, `==`/`!=`, `"x" + int` coercion.
- **Bools**: comparisons over all three scalar types, `and`/`or`/`not`.
- **Arrays**: int literals of generator-tracked length (the length is
  known to the generator and can be emitted as a literal; the stdlib-less
  differential pipeline has no `Array.length` binding), in-bounds
  indexing guarded via `(i*i) % len`.
- **Records**: literals, field access, functional update `{r .. f = v}`.
- **Control flow**: `if`/`then`/`else`, `match` on small ints, bounded
  `while` loops over `var` accumulators (15% are hot loops with trip
  counts 1050–1600 to force JIT tier-up inside a single run), `for x in
  [...]` loops.
- **Functions/closures**: top-level `fn` decls with inferred types,
  bounded recursion, calls from expressions.
- **Effects** (~12% of programs): `effect Tick { next: Int -> Int }` with
  a `handle`/`resume` wrapper around a bounded `while` loop — the resumed
  value crossing the JIT yield boundary is a known-hot correctness area.

The final expression is always Int/Float/Bool/String so the result is
cross-backend comparable.

## Running

```bash
# 10k programs (default), debug build:
scripts/difffuzz.sh

# 20-minute campaign:
scripts/difffuzz.sh --time 1200 --seeds 1000000

# Reproduce a single seed:
scripts/difffuzz.sh --seeds 1 --seed-base 0x0000000000001234

# Release build (faster per program, slower to build):
scripts/difffuzz.sh --release --seeds 50000
```

Note: the driver loads one JIT/AOT native module per program and resident
memory grows ~linearly with programs executed (~4 MB/program observed).
Under a small cgroup memory limit (4 GiB) a single process is OOM-killed
after roughly 900-1000 programs, so long campaigns must be *sharded*:
invoke the binary repeatedly with advancing `--seed-base` (e.g. 800 seeds
per shard). Crashers and stats accumulate across shards.

Environment: `CARGO_TARGET_DIR` (default `/tmp/ct-bfuzz`),
`NULANG_STDLIB` (default `/mnt/agents/nulang/src/stdlib`).

CI smoke tests:

```bash
# Reference VM + JIT/AOT coverage where enabled by this profile
cargo test --no-default-features difffuzz

# Adds the restricted WASM backend and requires nonzero WASM participation
cargo test --features wasm-backend difffuzz
```

Also see the pre-existing mutation-based fuzzer: `cargo test -- fuzz`
(`src/fuzz.rs`), and its `#[ignore]`d extended campaigns
(`fuzz_differential_extended`, shardable via `NULANG_FUZZ_ITERATIONS` /
`NULANG_FUZZ_SHARD_ID`).

## Crashers & triage

Divergences are persisted to
`fuzz/differential/crashers/seed_<hex>.nula` with a header comment:

```
// Differential fuzzer crasher
// seed: 81985529216486895 (0x123456789abcdef)
// interpreter/JIT divergence on ...: cold=... warm=...
// reproduce: nula_difffuzz --seeds 1 --seed-base 0x123456789abcdef
```

Triage procedure:

1. Reproduce: `scripts/difffuzz.sh --seeds 1 --seed-base 0x<hex>`.
2. The crasher file is itself a runnable Nulang program (the header is
   comments). Run it directly under each backend to isolate the
   disagreeing pair.
3. Minimize by hand: the generator is compositional, so sub-expressions
   usually reproduce. Confirm the minimized case still diverges, then add
   it as a regression test near the responsible backend's tests.
4. Classify: a divergence in value layout, NaN canonicalization, checked
   48-bit arithmetic, or error class is Frozen-tier surface — Sev-1 per
   PLAN.md Phase 1 kill criteria.

Compile failures (generated program rejected by the frontend) are printed
with their seeds at the end of a campaign; they indicate a generator bug
or an over-strict frontend and are not crashers.

## Findings

Campaign results are recorded here (newest first).

### 2026-08-16 — initial bring-up (feat/differential-fuzzing, base 59cd4c6)

#### Finding 1 (known class): 48-bit checked-overflow semantics differ across backends

The bytecode interpreter performs *checked* 48-bit arithmetic and raises
`integer overflow: `<op>` on A and B exceeds the 48-bit range ...` when an
`add`/`sub`/`mul`/`neg`/`pow` result leaves the 48-bit payload range. The
JIT and AOT backends do not agree with it (or with each other) on
overflowing programs:

- **interp vs JIT**: cold run raises the overflow error; the warm
  (tiered-up) run returns the wrapped value, e.g.
  `cold=Err("... overflow: `add` on -140737488355327 and -140737488355327 ...")`
  vs `warm=Ok("false")` (seed `3523149833` / `0xD1FF0009`, smoke run).
- **interp vs AOT**: interpreter raises; AOT returns the wrapped value,
  e.g. `interp=Err("... overflow: `mul` ...")` vs `aot=Ok("34674391791284")`
  (seed `3523149829` / `0xD1FF0005`). AOT *pins* wrapping semantics for int
  pow in `aot::codegen::tests::test_aot_float_pow_and_int_pow_overflow`
  ("int pow overflow wraps (wrapping_mul), not nil"), so at least part of
  this gap is intentional-but-undocumented design drift between backends.
- **JIT vs itself across tiers**: the JIT records the arithmetic error
  (`jit::runtime::record_arith_error`) but the tiered-up code continues
  with nil, turning a later `+` into
  `type error: arithmetic `+` requires numeric operands, got Int and Nil`
  where the interpreter raised overflow at the original op
  (seed `3523149839` / `0xD1FF000B`).
- **error-site drift**: both sides raise a 48-bit overflow error but name
  different ops (`add` vs `neg`, `mul` vs `pow`), i.e. the JIT/AOT detect
  overflow at a different point in the same expression
  (seed `3523149840` / `0xD1FF000C`).

Reproduce any of these: `scripts/difffuzz.sh --seeds 1 --seed-base 0xD1FF0009`
(the seed space changed when the generator's seed mixing was fixed to
`seed * 0x9E3779B97F4A7C15 ^ 0xA5A55A5AD3C3B4A5`; earlier handoff seeds
do not reproduce).

Classification: every program that executes an overflowing int op is a
divergence under the current backends, so the campaign classifies any
divergence whose oracle message contains `exceeds the 48-bit range` into
`CampaignStats::known_overflow` and persists it under
`fuzz/differential/crashers/known-overflow/`; the top-level crashers
directory remains reserved for untriaged classes. Per PLAN.md Phase 1,
checked 48-bit arithmetic is Frozen-tier surface — this is a Sev-1
correctness gap, not a fuzzer artifact. It needs a deliberate semantic
decision (checked-everywhere vs wrap-everywhere) plus a JIT/AOT
implementation pass; it is not fixable as a small generator-side change.

#### Finding 2 (fixed in generator): float division by zero — interp nil vs AOT value

An early campaign run (before the `nonzero_float` parenthesization fix)
generated `((0.1) / ((0.0) * (0.0)) + 1.0)`: the missing outer parens
made the divisor `(0.0) * (0.0)`, so the program performed a genuine
float division by zero. Result: the interpreter yields nil (poisoning the
surrounding `+` into `type error: arithmetic `+` requires numeric
operands, got Nil and Float`) while AOT returns a value and the program
completes (`aot=Ok("true")`) — seed 43 of the pre-fix seed space. The
crasher is preserved verbatim below because the generator's seed space
changed when this was fixed (the divisor is now `(((d) * (d)) + 1.0)`,
provably ≥ 1.0):

```nulang
let i0 = 587003
let s1 = (if true then (if true then "-1" else "foo bar") else "-1")
let f2 = (((0.1) / ((0.0) * (0.0)) + 1.0) * ((-2.5) / (if 0.1 == 0.0 then 1.0 else (0.1))))
let __final = true
__final
```

The generator no longer produces float div-by-zero, so this class is out
of campaign reach by construction; the backend disagreement itself (nil
vs value on `x / 0.0`) is the same root design question as Finding 1
(checked/erroring interp semantics vs wrapping/lax compiled backends).

#### Finding 3 (open, Sev-1): JIT loses `var` accumulator updates in `for x in [..]` loops

When the enclosing function tiers up, a `for` loop over an array literal
leaves the `var` accumulator at its initial value; the interpreter
computes the correct sum. Representative crashers (pre-fix seed space —
the crasher *files* are the repro artifacts, seeds no longer regenerate
them): seeds 135, 1928, 2070, 6794 (+ others under
`fuzz/differential/crashers/`). Minimized from seed 1928:

```nulang
let i2 = { var fa0 = 0
for x1 in [964593] { fa0 = fa0 + x1 }
fa0 }
i2
```

cold (interp) = `964593`, warm (JIT) = `0`. Variants show the stale
accumulator feeding downstream arithmetic (seed 135: cold `435260` vs
warm `720298`; seed 6794: cold `140736487854272` vs warm `-1000000000` —
both consistent with `i2` reading as 0). `while` loops over `var`s do NOT
exhibit this, so the bug is specific to for-loop lowering in a tiered-up
(JIT) region.

#### Finding 4 (open, Sev-1): AOT miscompiles float arithmetic, returning garbage

- seed 4940: final expr `(-(((if b4 then 1.5 else 1.5) * -67.459)))` —
  interp `101.1885`, AOT `-83633252455154`. Minimized: `(-((1.5 * -67.459)))`.
- seed 6468: `fn g6(p0) { (-((p0 + p0))) }` called with float
  `1000000000.5` — interp `-2000000001`, AOT `55641297125376`.
  Minimized: `fn g(p0) { -((p0 + p0)) }\ng(1000000000.5)`.
- seed 6657: interp `1.8990114746976543`, AOT `-55168688318720`.

The AOT results are large integers where the correct result is a small
float — looks like a tag/boxing error (float bits reinterpreted) rather
than a numeric-semantics difference, possibly involving unary `neg` on
floats or float arguments to untyped fn params.

#### Finding 5 (open): JIT warm run fails to terminate (step limit) where interp completes

seed 2781: cold = `false`, warm = `Err("step limit exceeded")`. The
program contains a bounded recursive fn and array guards; under the JIT
warm path the execution does not terminate within the interpreter step
limit.

#### Finding 6 (fixed in generator): unbounded generated recursion overflowed the native stack

Generated recursive fns count down from their first Int param
(`g(p0 - 1)` guarded by `p0 <= 0`). Later call sites passed arbitrary
Ints (up to ~10^6), giving ~10^6-deep recursion and a native stack
overflow that aborted the campaign process (18 of 28 shards in the first
campaign exited via SIGABRT). Fixed: calls to recursive fns now wrap the
first argument as `((arg) % 89)`. This changed the seed space again;
crashers from earlier runs are preserved as files and are their own
repro.

### 2026-08-16 campaign stats (sharded runs; shards cap at 800 seeds due to the in-process memory growth noted above)

- Seed spaces attempted: 0..22400 (run 1) and 100000..108000 (run 2); 20+ min wall total.
- Full-shard completions executed 800 programs in ~32-72s (debug build); ~28k seed positions attempted, with per-shard aborts (Finding 6) and OOM kills truncating many shards.
- Outcomes: 106 untriaged crashers persisted (Finding 3 for-loop class dominates; Findings 4/5 represented), 2690 known-overflow crashers under known-overflow/ (Finding 1). Zero compile failures of generated programs in completed shards.
- Smoke: 50 seeds, 37 agreed (24 with AOT), 13 known-overflow, 0 untriaged divergences.
