---
updated: 2026-09-22
sources:
  - AGENTS.md
  - SPEC2.md
  - src/main.rs
  - src/lib.rs
tags: [overview, architecture]
---

# Architecture Overview

Nulang is a distributed, actor-based programming language written in Rust (edition 2021, Cargo workspace with a primary `nulang` crate and optional support crates). It combines fault-tolerant actors, HM typing, reference capabilities, algebraic effects, a register-based bytecode VM, a Cranelift JIT, experimental WASM/native backends, and an optional AI runtime.

## Subsystem map

| Subsystem | Path | One-liner |
|-----------|------|-----------|
| Lexer/Parser | `src/lexer.rs`, `src/parser.rs` | Source → tokens → AST. |
| Type system | `src/typechecker.rs`, `src/types.rs` | HM Algorithm W + row-polymorphic records + effects. |
| Capabilities | `src/effect_checker.rs` | Pony-inspired lattice (iso/trn/ref/val/box/tag/lineariso). |
| HIR/MIR | `src/hir_lower.rs`, `src/mir_lower.rs`, `src/mir_codegen.rs` | AST → HIR → MIR → bytecode (MIR-exclusive pipeline). |
| Bytecode VM | `src/vm.rs`, `src/bytecode.rs`, `src/value_layout.rs` | 256-register frames, i64-tagged values, 135 opcodes. |
| JIT | `src/jit/` | Cranelift-backed hot-region compilation, typed + SIMD tiers. |
| Actor runtime | `src/runtime/` | Sharded cooperative scheduling, ORCA GC, supervision, mailboxes. |
| Distribution | `src/runtime/network.rs`, `cluster.rs`, `distributed.rs` | NUL0 TCP wire protocol, gossip membership, remote spawn. |
| CRDTs | `src/runtime/crdt.rs`, `crdt_reg.rs`, `crdt_manager.rs` | 8 CRDT types with delta-state replication. |
| WASM backend | `src/mir_wasm.rs`, `src/wasm_runtime.rs` | MIR → WASM via `wasm-encoder`; Wasmtime host runtime. |
| AOT backend | `src/aot/` | MIR → Cranelift CLIF → native object code. |
| AI runtime (pure) | `crates/nulang-ai/` | LLM providers, memory, pipelines, debates, supervisors — zero core deps. |
| AI runtime (integration) | `src/runtime/ai_impls.rs`, `agent.rs`, `llm.rs` | Trait impls, agent completion pipeline, LLM worker thread. |
| LSP | `src/lsp/` | 12-feature `tower-lsp` server. |
| Python interop | `src/python/` | PyO3 abi3 bridge. |
| C FFI | `src/ffi/` | Stable C embedder API. |
| Package manager | `src/package/` | `nula` — manifest, lockfile, resolver, commands. |
| Format layer | `src/format/` | Frozen `.nbc` bytecode, NUL0 wire versioning, migration registry. |

## Execution backend model

The frontend (lexer → parser → typechecker → effect/capability checker → HIR → MIR) is shared. The bytecode VM is the semantic reference; other backends must prove parity against it.

1. **Bytecode VM** (default): MIR → bytecode → register VM, with Cranelift JIT tiering for hot regions.
2. **WASM** (`--backend wasm|wasm-run|wasm-aot`, requires `wasm-backend`): MIR → `.wasm` via `wasm-encoder`, executed by Wasmtime. The experimental `wasm-component` path emits WIT alongside WASM.
3. **Native AOT** (`--backend native`): MIR → Cranelift CLIF → native object code. This remains secondary until semantic parity is demonstrated.

## Concurrency model

There is **no async/await in the VM or actor runtime.** Actor execution is cooperative and reduction-yielding. A `Runtime` owns one shard (actors partition by `actor_id % shard_count`) and one live scheduler thread; `NULANG_SHARDS>1` runs multiple shards in parallel with bounded `mpsc::SyncSender` cross-shard channels. The scheduler retains Chase-Lev peer-stealing APIs for alternate/future multi-worker callers, but the current live per-shard `run_scheduler()` path uses worker slot 0 rather than peer stealing.

The only async surfaces are `main.rs` (`#[tokio::main]`), the LSP server (`tower-lsp` over tokio stdin/stdout), and the AI LLM client (`async_trait`, exposed to sync callers via `complete_sync`).

## Actor lifecycle (short version)

Spawn → schedule (priority queues High/Normal/Low on the shard owner) → step (mailbox dequeue → handler dispatch → reduction budget → yield or continue) → GC (ORCA delta ops + incremental cycle detection) → fault (link/monitor propagation, supervisor restart strategies).

For the full protocol see [[../subsystems/actor-runtime]] _(to be created on next ingest of `src/runtime/`)_.

## What to read next

- Compiler stages: [[compiler-pipeline]].
- Language semantics: `SPEC2.md`.
- Architecture contract: `AGENTS.md` (authoritative, denser than this page).
- Stability tiers and RFC process: `GOVERNANCE.md`.

## Source citations

- Subsystem inventory: `AGENTS.md` (Key Directories section).
- Compiler pipeline: `AGENTS.md` (Architecture & Data Flow section).
- Concurrency model: `AGENTS.md` (project overview + runtime lifecycle sections).
- Backend selection: `src/main.rs` (`--backend` flag handling), `src/mir_codegen.rs`, `src/mir_wasm.rs`, `src/aot/codegen.rs`.
