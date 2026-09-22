# Backend Profiles

> Generated from `spec/backend-profiles/v0alpha1.json`. Do not edit this table by hand.

Semantic reference: **bytecode**.  
Canonical portable/cloud target: **wasm**.

| Backend | Maturity | Role | Fallback | Known semantic restrictions |
|---|---|---|---|---|
| `bytecode` | stable | semantic-reference | — | None |
| `jit` | stable | optimization | `bytecode` | None |
| `wasm` | experimental | portable-cloud | — | `user-defined-effect-handlers`, `continuation-resume` |
| `wasmfx` | experimental | stack-switching-research | — | `user-defined-effect-handlers`, `continuation-resume` |
| `native` | experimental | secondary-aot | — | `continuation-resume`, `full-language-parity-not-guaranteed` |

A restricted backend must reject unsupported semantics explicitly; it must never silently reinterpret them. JIT is an optimization profile and may fall back to the bytecode interpreter for unsupported hot regions.
