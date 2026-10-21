# Contributing to Nulang

Thanks for your interest in contributing! Nulang is pre-1.0 alpha software —
expect rough edges, and expect breaking changes before v1.0.

## Build from source

```bash
git clone https://github.com/nulang-org/nulang.git
cd nulang
cargo build --release
```

Requires Rust 1.95.0 (pinned by `rust-toolchain.toml`), Linux or macOS.
Windows is unsupported; use WSL.

## Verify the first-run developer workflow

A native CLI build can exercise the end-user path without modifying the repository:

```sh
cargo build --locked --bin nulang
python3 scripts/smoke_first_run.py --bin /tmp/cargo-target/debug/nulang
```

The smoke creates a disposable `lib` project and checks CLI version, inline
evaluation, scaffolding, type checking, compilation to a **nonempty** bytecode
artifact, execution, a nonempty passing test suite, and formatting idempotence.
It does not publish, deploy, access remote registries, or leave a generated
project behind. Use a path to the actual compiled binary: the source
`.cargo/config.toml` redirects Cargo's default target directory.

The same command runs in the CI **Nula Package Test** job against the exact
release-profile binary produced by the Release Build job. It cannot run if
that upstream job fails; a skipped smoke is not passing evidence. The Python
fixture tests run through `python3 -m unittest discover -s scripts/tests
-p 'test_*.py'`, but they do not replace a real binary smoke.

## Run the tests

```bash
cargo test                                          # full test suite
cargo test --features wasm-backend                  # WASM backend suite
./conformance/run.py                                # behavioral conformance suite
NULANG_BIN=./target/release/nulang bash scripts/verify_doc_examples.sh  # doc snippets
```

Please make sure these pass before opening a PR. CI runs them all.

## Commit style

We use [Conventional Commits](https://www.conventionalcommits.org/):
`feat:`, `fix:`, `docs:`, `chore:`, `refactor:`, `test:`, etc., with a short
imperative summary. PR titles are checked by CI.

## Core Admission Rule

A feature enters the language core **only if it cannot be implemented as**:

- a **library** (user-space `.nula` code),
- a **capability package** (an effect module behind a capability),
- a **compiler plugin** (a lowering/lint pass outside the frozen pipeline),
- a **WASM component** (a WIT-composable module), or
- a **cloud service** (something the platform provides at runtime).

If any of those five vehicles can carry the feature, the feature does not
belong in the kernel — propose it there instead. Rationale: we keep a
**tiny kernel and an enormous platform**. Every construct in the core is
part of the Frozen/Stable surface forever (see GOVERNANCE.md), imposes
compiler, runtime, and spec maintenance cost on every user, and constrains
every future optimization. Features shipped as libraries, packages,
plugins, components, or services can evolve, be deprecated, and be replaced
without touching the language.

## Stability tiers

Every public surface is Frozen, Stable, or Experimental (see
[GOVERNANCE.md](GOVERNANCE.md)). Bug fixes and Experimental features don't
need an RFC; changes to Frozen or Stable surfaces do — see
`RFC/0000-template.md`. Record every user-visible change in `CHANGELOG.md`
under the correct tier.

## Questions?

- **Ask / discuss:** [GitHub Discussions](https://github.com/nulang-org/nulang/discussions)
- **Bugs & features:** [open an issue](https://github.com/nulang-org/nulang/issues/new/choose)

By participating you agree to abide by our [Code of Conduct](CODE_OF_CONDUCT.md).
