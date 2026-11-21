# Rust build performance

Nulang has several intentionally heavyweight optional subsystems: Cranelift native code generation, PyO3, libSQL, the LSP server, the AI runtime, Wasmtime, and network/FFI integrations. The normal default build continues to include the production feature set. During compiler development, prefer a narrower build whenever the subsystem being changed does not require those features.

## Fast inner loop

Use the Cargo aliases in `.cargo/config.toml`:

```bash
cargo fast-check        # minimal library type-check
cargo fast-test         # minimal library tests
cargo fast-build        # minimal runnable build
cargo check-jit         # minimal library + Cranelift native codegen
cargo check-full        # workspace/all targets/all features validation
cargo build-timings     # emit Cargo's build timing report
```

For parser, typechecker, HIR/MIR, bytecode, VM, effects, capabilities, and most runtime work, start with `cargo fast-check` or `cargo fast-test`. Use the full feature graph before merging changes that can interact with optional integrations.

## Faster Linux linking

The repository no longer forces GNU `bfd`. The checked-in Cargo configuration keeps the portable system linker as the default so a contributor does not need mold or LLD just to build Nulang.

When mold or LLD is installed, use the wrapper:

```bash
bash scripts/cargo-fast.sh
bash scripts/cargo-fast.sh test --lib --no-default-features
bash scripts/cargo-fast.sh build
```

The wrapper selects, in order:

1. `mold`
2. LLD (`ld.lld`/`lld`)
3. the system linker

Override automatic selection when benchmarking:

```bash
NULANG_FAST_LINKER=system bash scripts/cargo-fast.sh build
NULANG_FAST_LINKER=mold   bash scripts/cargo-fast.sh build
NULANG_FAST_LINKER=lld    bash scripts/cargo-fast.sh build
```

The wrapper retains Nulang's Linux `--export-dynamic` linker requirement.

## Development profiles

The normal `dev` profile is optimized for compiler throughput:

- workspace code uses `opt-level = 0`
- workspace code keeps line-table debug information for useful backtraces
- dependencies omit debug information
- dependencies are not optimized by default
- incremental compilation remains enabled

If generated debug binaries are too slow for runtime-heavy testing, use the opt-in profile that restores dependency `opt-level = 1`:

```bash
cargo dev-runtime-build
# or
cargo build --profile dev-runtime
```

For interactive native debugging with full symbols for workspace code:

```bash
cargo debug-build
# or
cargo build --profile debugging
```

## Measuring changes

Do not judge build-performance work from one warm build. Record at least these cases:

```bash
cargo clean
/usr/bin/time -p cargo fast-check

/usr/bin/time -p cargo fast-check       # warm/no-op
/usr/bin/time -p cargo check-jit
/usr/bin/time -p cargo check-full

cargo clean
cargo build-timings
```

For incremental performance, edit one representative file in each compiler stage and record the next `cargo fast-check`:

- `src/parser.rs`
- `src/typechecker.rs`
- `src/hir_lower.rs`
- `src/mir_lower.rs`
- `src/vm.rs`

This separates cold dependency cost from Rust crate invalidation cost.

## Cloud and ephemeral builders

Long-lived local development should normally keep Cargo incremental compilation enabled. Ephemeral Nulang Cloud/CI workers should prefer a shared compiler cache such as `sccache` and disable rustc incremental compilation for the cached build:

```bash
CARGO_INCREMENTAL=0 RUSTC_WRAPPER=sccache cargo check --workspace
```

The remote cache key/storage policy belongs in the cloud build layer rather than the repository's default Cargo configuration, because local incremental compilation and remote compiler caching optimize different workloads.

## Next architectural step

The remaining large compile-time bottleneck is the size of the root `nulang` crate. The compiler frontend, semantic analysis, IR, bytecode VM, native codegen, WASM backend, LSP, package tooling, and FFI currently share a large compilation unit. Once timing data confirms the invalidation hot spots, split along stable stage boundaries rather than creating many tiny crates blindly.

A likely dependency direction is:

```text
syntax -> sema -> ir -> bytecode -> vm
                    |-> codegen-cranelift
                    |-> codegen-wasm

runtime -> vm
lsp -> syntax + sema
cli -> selected backends/runtime
```

The goal is that editing the parser or typechecker does not rebuild/link PyO3, Wasmtime, the AI runtime, or unrelated integration layers.
