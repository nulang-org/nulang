# Renderer-neutral UI protocol ownership boundary

Status: **source-ownership ratchet only**. This follows RFC 0033 and does not change source-language semantics, Cargo feature profiles, artifact formats, or the Web application API.

## Current main architecture

- `nulang-ui-protocol` is currently an unconditional root Cargo dependency. It has `serde` and `serde_json` dependencies, also used elsewhere by Nulang.
- The direct `nulang_ui_protocol` Rust references in the root `src/` tree currently occur in `src/web/http_request.rs`, `src/web/action_dispatch.rs`, and `src/web/reactivity.rs`.
- `src/web` is unconditional in `src/lib.rs`. Its HTTP request paths are called by `src/runtime/http_server.rs`, and reactivity/other Web tooling is called by `src/package/commands.rs`.
- The minimal build therefore still includes the UI protocol crate and Web support. A code search cannot demonstrate that this is all removable without changing external behavior.

## First enforceable rule

`scripts/check_ui_protocol_ownership.py` rejects any `nulang_ui_protocol` token in Rust files under `src/` outside `src/web/`. The checker preserves source locations, ignores ordinary Rust comments, and fails on an absent source tree or unreadable source. This is deliberately a **simple structural ratchet**, not a complete Rust parser. It does **not** establish that `src/web` itself is isolated, that third-party dependencies disappear, or that compiled output has no UI components.

The existing minimal CI job runs the Python unit tests and guard before Cargo compilation. It does not skip or override any existing checks.

## Safe future extraction sequence

1. Keep Web handlers and action authorization/replay protections unchanged. Inventory all `src/web` callers, its public API, and existing Web request/form/action tests.
2. Propose one `ui-protocol` cargo feature (or more specific feature split) only after defining exactly which public API/behavior becomes unavailable in minimal builds. Decide whether API compatibility requires a harmless always-available protocol *interface* while the optional implementation lives in an adapter.
3. Make the renderer-neutral UI protocol optional, with no silent fallback that would accept unvalidated form action envelopes. Unsupported explicit protocol messages must fail closed. Evaluate feature wiring across root `Cargo.toml`, `src/web/mod.rs`, HTTP request capture, action dispatch, reactivity, server routing, and `nula` package commands.
4. Prove minimal, release-cli, full, and WASM builds/tests and Web conformance on the **exact same head**. Verify no public form/action authorization or durable identity behavior regresses.
5. Compare Linux target-filtered dependency closure to the measurements from #2288 and report **actual delta** (expected potentially small, because `serde` is shared).
6. Consider changing default feature profiles only under a separate compatibility RFC after adoption and release gates are proven.

Do not try to eliminate `serde` or implement a new JSON parser merely to reduce a raw package count.

## Validation

From the repository root:

```shell
python3 -m unittest scripts/tests/test_check_ui_protocol_ownership.py
python3 scripts/check_ui_protocol_ownership.py
```

Treat passing this checker as evidence of **current source ownership only**, not an accepted or production-ready Web feature extraction.
