# Nulang code intelligence (local-first)

## Semantic sources of truth

- **Rust implementation**: use `rust-analyzer` for go-to-definition, references and rename. Inspect feature-gated code explicitly and resolve workspace structure with `cargo metadata --locked --no-deps --format-version 1`.
- **Nulang-language files**: use the project's existing native LSP (`nulang --lsp`) to inspect symbol definitions, cross-file references, renames and diagnostics; it knows more about Nulang source than an off-the-shelf Rust or TypeScript parser.
- **Supplementary graph**: `codebase-memory-mcp` (MIT) is wired into the project's Codex MCP configuration for local code navigation. This is optional and must not become a new CI dependency or promotion gate.

## Setup

Install a reviewed native release of [Codebase Memory MCP](https://github.com/DeusData/codebase-memory-mcp) on the developer's machine and add its executable to `PATH`. Restart Codex. To index the checkout:

```bash
codebase-memory-mcp cli index_repository --repo-path "$(pwd)"
codebase-memory-mcp cli list_projects
```

Use its `index_status`, `check_index_coverage`, `search_graph`, `trace_path` and `detect_changes` MCP tools. If the executable is not present, set `enabled = false` in `.codex/config.toml` and continue using `rust-analyzer`, `cargo metadata`, `rg`, the native LSP and compiler tests. The index is local by default; no graph artifact is committed.

## Impact and verification

A missing/partial graph, no callers found, or unresolved dynamic edges never proves the code is unused. In particular, actor-message routing, CRDT and durability effects, bytecode recovery, compiler JIT/native/WASM feature gates and protocol compatibility require executable evidence. Validate changes with the repo's appropriate Rust tests, conformance suite, differential/compiler tests and targeted benchmarks. No compiler or runtime behavior is changed by this configuration PR.

Optionally compare [CodeGraph](https://github.com/colbymchenry/codegraph) in a separate developer session; do not run multiple overlapping agents automatically or introduce graph indexing into the required CI path without demonstrating net benefit.
