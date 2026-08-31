<p align="center">
  <img src="docs/src/assets/logo.svg" width="120" alt="Nulang logo">
</p>
<h1 align="center">Nulang</h1>
<p align="center">
  An actor-based language with algebraic effects, capability-based types, and durable/distributed actors for building resilient software.
</p>
<p align="center">
  <a href="https://nulang.org">Website</a> •
  <a href="playground/">Playground</a> •
  <a href="https://nulang.cloud">Nulang Cloud</a> •
  <a href="https://github.com/nulang-org/nulang">GitHub</a>
</p>
<p align="center">
  <a href="https://www.rust-lang.org"><img src="https://img.shields.io/badge/rust-2021%20Edition-orange.svg" alt="Rust 2021"></a>
  <a href="https://github.com/nulang-org/nulang/blob/main/LICENSE"><img src="https://img.shields.io/badge/license-Apache%202.0-blue.svg" alt="License Apache 2.0"></a>
  <a href="https://github.com/nulang-org/nulang/actions"><img src="https://github.com/nulang-org/nulang/workflows/CI/badge.svg" alt="CI"></a>
  <a href="https://codecov.io/github/nulang-org/nulang"><img src="https://codecov.io/github/nulang-org/nulang/graph/badge.svg" alt="Coverage"></a>
<a href="https://github.com/nulang-org/nulang/actions/workflows/docs-sync.yml"><img src="https://github.com/nulang-org/nulang/actions/workflows/docs-sync.yml/badge.svg" alt="Docs Sync"></a>
  <a href="https://deepwiki.com/nulang-org/nulang"><img src="https://img.shields.io/badge/DeepWiki-docs-blue.svg" alt="DeepWiki"></a>
</p>

---

## What is Nulang?

Nulang is an actor-based programming language with algebraic effects and
capability-based types. It fuses Erlang-style fault-tolerant actors with a
Hindley-Milner type system, reference capabilities (`iso`/`trn`/`ref`/`val`/`box`/`tag`/`lineariso`),
and row-polymorphic algebraic effects. The compiler pipeline (AST → HIR → MIR)
targets a register-based bytecode VM with a Cranelift JIT, an ahead-of-time
native backend, and an optional WASM backend. The runtime is a multi-threaded
work-stealing executor with supervision trees, ORCA garbage collection,
location-transparent distribution, and durable persistence.

---

## Installation

### Pre-built binaries
Download the latest release from [GitHub Releases](https://github.com/nulang-org/nulang/releases)
*(prebuilt binaries coming with the first tagged release — for now build from source below)*.
- **Linux (x86_64)**: `nulang-linux-x86_64.tar.gz`
- **Linux (aarch64)**: `nulang-linux-aarch64.tar.gz`
- **macOS (x86_64)**: `nulang-macos-x86_64.tar.gz`
- **macOS (aarch64)**: `nulang-macos-aarch64.tar.gz`

Extract and place `nulang` in your PATH:
```bash
tar xzf nulang-linux-x86_64.tar.gz
sudo mv nulang /usr/local/bin/
```

### From source
```bash
git clone https://github.com/nulang-org/nulang.git
cd nulang
cargo build --release
```
Requires Rust 1.95.0 (pinned by `rust-toolchain.toml`), Linux or macOS. Windows is not supported yet — use [WSL](https://learn.microsoft.com/windows/wsl/) and build inside a Linux environment.


## Quick Start

**Prerequisites:** Rust 1.95.0, Linux or macOS. Windows is unsupported for now — Windows users should build under WSL.

```bash
git clone https://github.com/nulang-org/nulang.git
cd nulang
cargo build --release
```

```bash
nulang hello.nula              # compile + run
nulang --check hello.nula      # type-check only
nulang --eval '40 + 2'         # evaluate inline code
nulang --repl                  # interactive REPL
```

A Nulang program:

```nulang
perform IO.print("Hello, Nulang!")

let name = "World"
perform IO.print("Hello, " + name + "!")
```

> Run `nulang examples/01_hello.nula`. See [`examples/`](examples/) for
> 17 verified programs covering actors, effects, pattern matching, records,
> loops, arrays, HTTP, JSON, and more.
>
> No install? Try the [`playground/`](playground/) — run it locally with
> `python3 playground/server.py` (a hosted version at nulang.org/playground
> is coming soon).

---

## Feature Highlights

- **Algebraic effects** — `perform Effect.op(args)` / `handle body with { | Effect.op(x) => ... }` with resume semantics. Effect dependencies are explicit in function signatures via `!` rows.
- **Capability-based types** — `iso`, `trn`, `ref`, `val`, `box`, `tag`, and `lineariso` guarantee memory safety and data-race freedom. Checked at compile time; erased at runtime.
- **Hindley-Milner type inference** — full Algorithm W with row-polymorphic records, variant types, and algebraic effect rows.
- **Actors** — `spawn`, `send`/`!`, `ask`, selective `receive` with `after` timeout, links, monitors, supervision trees, process groups, and actor priority scheduling.
- **Entities & workflows** — `entity` declarations (durable-first, event-sourced by default). `workflow` declarations with steps, timers, signals, and saga compensation that survive restarts.
- **`let` and `var`** — immutable and mutable bindings. Records with `{ field: value }` syntax and `{ base .. field = new_val }` update syntax. Pattern matching with guards, alias patterns, and recursive sub-patterns. `**` exponentiation. Multi-line `"""..."""` strings with `\u{...}` unicode escapes. Pipe operator `|>`.
- **Error handling** — `catch expr fallback` (prefix or postfix), `fail Error(...)` for structured short-circuit return, `T ! E` return types, `?` unwrap.
- **FS file I/O** — `perform FS.read(path)`, `perform FS.write(path, content)`, `perform FS.append(path, content)`, `perform FS.exists(path)`.
- **Package manager** — `nula new/init/build/run/test/add/remove/list/clean/doc`. See [below](#package-manager).
- **Test runner** — `nula test` discovers `.nula` files under `tests/`; uses the `Test` effect (`perform Test.assert_eq(a, b)`, `perform Test.assert(cond, msg)`, etc.).
- **LSP server** — `nulang --lsp` with diagnostics, hover, goto-definition, references, rename, completion, inlay hints, formatting, signature help, and semantic tokens.
- **REPL** — `nulang --repl` with `:help <topic>`, `:type <expr>`, `:load <file>`, tab completion, and automatic multi-line input.
- **AI runtime** — `agent` declarations, LLM providers (OpenAI, Ollama), episodic/semantic/procedural memory, pipelines, debates, and supervisor teams. Gated behind the `ai-runtime` feature flag. *Experimental.*
- **Distribution** — location-transparent `send`/`ask` over TCP (NUL0 wire protocol) and gossip membership. *Experimental.* The 8 CRDT types (`GCounter`, `ORSet`, …) are implemented and tested at the Rust embedder level only — `.nula`-level `state crdt` fields are not yet wired to them and behave as `durable` (see SPEC2 §9.10).
- **WASM backend** — MIR→WASM compilation via `--backend wasm|wasm-run|wasm-aot`, Wasmtime host runtime with guard pages and SIMD. Gated behind the `wasm-backend` feature flag. *Experimental.*
- **AOT native backend** — `--backend native` compiles pure-functional programs (no effects, actors, or FFI) to native code via Cranelift; other constructs fail with a specific "not yet supported in the native backend" error naming the construct. Use the default `bytecode` backend for full-language programs. *Experimental.*

---

## Documentation

| Document | Description |
|----------|-------------|
| [`docs/GETTING_STARTED.md`](docs/GETTING_STARTED.md) | Installation, values, effects, actors, pattern matching — with runnable code snippets |
| [`docs/TUTORIAL.md`](docs/TUTORIAL.md) | **Tutorial:** Build a Weather CLI step by step — variables, functions, HTTP, JSON, pattern matching, records, file I/O |
| [`docs/PITFALLS.md`](docs/PITFALLS.md) | Common mistakes: `::` vs `.`, `let` vs `var`, `perform` keyword, `catch`/`fail`, record syntax, and more |
| [`examples/`](examples/) + [`README`](examples/README.md) | 17 verified, self-contained example programs |
| [`SPEC2.md`](SPEC2.md) | Language specification: syntax, semantics, type system, runtime, format stability contract |
| [`CHANGELOG.md`](CHANGELOG.md) | Changelog organized by stability tier (Frozen / Stable / Experimental) |
| [`GOVERNANCE.md`](GOVERNANCE.md) | Stability tiers, RFC process, and language versioning |
| [`ARCHITECTURE.md`](ARCHITECTURE.md) | Implementation architecture and module map |
| [`editors/vscode/`](editors/vscode/) | VS Code extension (syntax highlighting, language essentials, snippets) — build a `.vsix` or install manually |
| [`RFC/`](RFC/) | RFC proposals (format stability, frozen core, deprecation cycles, roadmap) |

### Docs auto-sync and DeepWiki

Changes to `src/**`, `examples/**`, `scripts/**`, `docs/**`, or the
[`.github/workflows/docs-sync.yml`](.github/workflows/docs-sync.yml) workflow
trigger a docs regeneration run on every push to `main`. The workflow regenerates
the derived standard-library pages (`docs/src/content/docs/stdlib/`) and the
full API reference (`docs/api.md`), commits any changes back to `main` with
`[skip ci]`, validates the Astro site build, and pings
[DeepWiki](https://deepwiki.com/nulang-org/nulang) as a best-effort nudge to
re-index the repository docs. The site is then redeployed automatically by the
Cloudflare Pages Git integration.

---

## Package Manager

Nulang ships with `nula`, a package manager invoked as `nulang nula <subcommand>`:

```bash
nulang nula new my-app       # scaffold a new package
nulang nula init             # initialize a package in the current directory
nulang nula build            # resolve dependencies + type-check
nulang nula run              # build and run the entry point
nulang nula test             # discover and run tests/ directory
nulang nula add <name>       # add a dependency (--path, --git, --version)
nulang nula publish          # publish the package to a registry
nulang registry serve        # run a local package registry server
nulang nula remove <name>    # remove a dependency
nulang nula list             # list locked dependencies
nulang nula clean            # remove build artifacts
nulang nula doc              # generate Markdown API docs
```

---

## Project Status & Stability

Nulang is **alpha software**. The language version is `1.0.0-frozen`
(RFC 0001/0002). Every public surface is classified into one of three tiers
(see [`GOVERNANCE.md`](GOVERNANCE.md) for the full definitions):

| Tier | Scope |
|------|-------|
| **Frozen** | Never breaks — `.nbc` bytecode format, NUL0 wire protocol, value layout, Nulang Core, and the `IO`/`Spawn`/`Send`/`Receive` built-in effects. |
| **Stable** | HM type system, effect rows, capability lattice, actor surface. Breaking changes require an RFC and a deprecation cycle. |
| **Experimental** | Everything else — feature flags (`wasm-backend`, `python`, `sqlite`, `lsp`, `ai-runtime`), distribution (multi-node `send`/`ask`, CRDTs — Rust-level only so far), and items marked Experimental in [`CHANGELOG.md`](CHANGELOG.md). |

> **Pre-1.0 disclaimer:** Nulang does not have external users yet. The tier
> guarantees above are the maintainer's stated policy and intent, but **expect
> breaking changes before v1.0** — any guarantee may be revised until the
> language sees real-world use.

1550+ tests pass with `cargo test`. Add `--features wasm-backend` for the
WASM backend test suite.

---

## Nulang Cloud

**[Nulang Cloud](https://www.nulang.cloud)** is an optional managed platform
for running Nulang actors in production — auto-scaling, zero cold start,
managed durability, and location-transparent messaging across regions.

The language and runtime in this repository are **Apache-2.0** and fully
self-hostable. No lock-in.

---

## Docker

A multi-stage Docker image is available. Build and run:

```bash
docker build -t nulang .
docker run --rm nulang --eval 'perform IO.print("Hello from Docker!")'
```

The image is ~50 MB and contains only the `nulang` binary and its runtime
dependencies.

## Community

- **Questions & discussion:** [GitHub Discussions](https://github.com/nulang-org/nulang/discussions)
- **Bugs & feature requests:** [Issue tracker](https://github.com/nulang-org/nulang/issues/new/choose)
- **Contributing:** see [CONTRIBUTING.md](CONTRIBUTING.md) and our [Code of Conduct](CODE_OF_CONDUCT.md)

---

## Docs & Wiki

The [nulang.org](https://nulang.org) documentation site is regenerated automatically on every push to `main` via the [Docs Sync workflow](.github/workflows/docs-sync.yml). When source files, examples, or docs content change, the workflow regenerates the standard-library reference and API docs, commits the updates, verifies the Astro build, and pings [DeepWiki](https://deepwiki.com/nulang-org/nulang) to encourage re-indexing.

---

## License

Nulang is licensed under the [Apache License, Version 2.0](https://github.com/nulang-org/nulang/blob/main/LICENSE).

Copyright 2026 © David Porkka
