# Compiler-stage benchmark

This benchmark measures Nulang's **isolated-module frontend and bytecode pipeline**.
It provides per-stage samples before changing the compiler's caching or
invalidation behavior. It does **not** measure a complete `nulang` CLI build
or a native executable build.

## Run

Build once, then execute the prebuilt runner to exclude Cargo/Rust compilation:

```sh
cargo build --release --no-default-features --bin nulang-compile-bench
./target/release/nulang-compile-bench --format jsonl --repeat 20
./target/release/nulang-compile-bench --list
./target/release/nulang-compile-bench --workload functions_100 --repeat 20
```

The zero-optional-feature profile isolates the frontend and bytecode compiler
from unrelated Python, networking, AI, and Cranelift build dependencies. This
choice changes the build of the *benchmark harness*, not the language
semantics being measured.

Run the automated correctness checks:

```sh
cargo test --no-default-features --bin nulang-compile-bench
```

Fixtures include a scalar expression, 10 and 100 independent typed functions,
and a branch-heavy loop. The function fixtures stress declaration analysis;
the loop stresses compiler control-flow lowering. Each repetition performs a
**fresh lex/parse/check/lower/bytecode compilation in the same warmed process**
with no incremental cache. The program is not executed.

## JSONL contract (schema 1)

Each record has `workload`, `iteration`, `source_bytes`, `backend`,
`scope`, `total_ns`, `stage_sum_ns`, and `stages_ns` containing:

- `lex` and `parse`
- `typecheck`
- `effectcheck` and `capability`
- `hir`, `mir`, and `bytecode`

`total_ns` encloses all stages; `stage_sum_ns` is the sum of the individual
timed regions. They intentionally differ due to bookkeeping, allocations,
and other untimed overhead. Timings use a monotonic clock and **must never be
asserted against absolute time limits in shared CI**.

## Scope and exclusions

This runner uses the same core lexer, parser, type checker, effect checker,
capability analyzer, HIR/MIR lowering, and bytecode compiler as the CLI, but
only runs on self-contained fixtures. It does not inject the standard prelude,
resolve imported modules, perform route analysis, rewrite client signals,
serialize an `.nbc` artifact, link native objects, start the VM, or JIT-compile
hot regions. It checks capabilities for top-level functions (fixtures have no
actors or workflows). Do **not** claim these timings represent the entire CLI
compilation path.

To compare with Go meaningfully, additionally measure *external* clean native
build wall time, incremental edit-to-runnable time, peak RSS, final artifact
size, and program runtime with matched workloads, hardware, optimization,
toolchain cache policy, and output semantics. Go native `go build` is not
equivalent to Nulang bytecode generation.

## Next optimization experiment

Use a saved set of local results as baseline before implementing a
content-addressed semantic cache. Invalidation must include the source file,
transitive imported interfaces, compiler version, target, feature/optimization
settings, prelude version, and effect/capability configuration. Changing any
of these must invalidate affected results. Cache hits must produce diagnostics
and artifacts semantically equivalent to uncached builds, including preserved
diagnostic source locations. Do not use source-only hashes as a shortcut for
these dependencies.

Prefer piloting a module-boundary semantic cache before attempting
function-level invalidation or native AOT caching.
