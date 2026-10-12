# Nulang RFC 0003 — Infrastructure Phase Completion Report

> **Historical snapshot:** this report records the repository state on 2026-08-09. It is not a current release-health dashboard. Use the required CI checks on the exact commit being tagged, together with `docs/RELEASE_CHECKLIST.md`, for current release readiness.

**Date:** 2026-08-09
**Commits:** 4 pushed to origin/main
**Tests:** 1679 passing (+12 new), 1 pre-existing AOT failure

## RFC 0003 Items Delivered

| # | Item | Status | Evidence |
|---|---|---|---|
| 2 | Formal semantics | 8 Lean modules, `shift_compose` lemma (12/13) | `lake build` passes |
| 3 | Self-hosting bootstrap | Stage 1+2 verified | `verify.sh` 11/11, emitter 5 programs |
| 5 | LLM decouple | Breaking phase complete | `LlmAsk` removed, `Provider.ask` canonical |
| 6 | Backend trait wiring | 8 traits trait-erased | `create_default_jit()` factory |
| 10 | Runtime god-object | Supervisor teams extracted | `supervisor_registry.rs` |
| 11 | Content-addressed modules | BLAKE3 lockfile hashes | `Nulang.lock` content_hash |
| 14 | Transport hygiene | Verified complete | rustls/quinn/reqwest confined |
| 15 | Trace context | `trace_id` on `Message` | Wire→cross-shard→local |
| 16 | WASM WIT mapping | 6 tests | `src/witgen.rs`, `--backend wasm-component` |
| 17 | .nbc library distribution | Export table, manifest, resolver | `--emit-nbc` pipeline verified |

## Bootstrap Programs Verified

| # | Program | Result | entry_point |
|---|---------|--------|-------------|
| 0 | Literal 42 | 42 | 0 |
| 1 | `1 + 2` | 3 | 0 |
| 2 | `if 1<2 then 10 else 20` | 10 | 0 |
| 3 | `double(21)` via function call | 42 | 3 |
| 4 | `fact(6)` recursive | 720 | 11 |

## Key Discoveries

- `entry_point` is direct instruction offset (not function table index)
- PC pre-incremented before opcode dispatch
- `ICmpLe = 0x43` (not 0x44)
- Jump offset = `target - pc` (accounts for pre-increment)
- Recursion proven working in VM
- Weakening requires de Bruijn indices (named representation needs freshness)

## Security

- wasmtime 46.0.1→46.0.2 (RUSTSEC-2026-0222, low) — **fixed**
- quick-xml 0.14.0 (RUSTSEC-2026-0194/0195, high) — blocked by inferno 0.7.0
- 8 unmaintained crate warnings (ansi_term, atty, etc.)

## Remaining Work

| Task | Effort | Dependencies |
|------|--------|-------------|
| Formal semantics proofs | 2-4 weeks | Clean `SyntaxDB.lean` rewrite |
| Bootstrap Stage 3 | 2-4 weeks | Minimal Core VM |
| AOT SIMD regression | 1-2 days | SIMD commit author |
| quick-xml upgrade | 1-2 days | inferno version bump |
