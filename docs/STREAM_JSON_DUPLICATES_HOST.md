# JSONL duplicate-key rejection at the parsing boundary

This experimental Rust-host improvement is stacked on
[#2073](https://github.com/nulang-org/nulang/pull/2073), itself a child of
#2072 → #2067 → #2065.

The `JsonlRecords` iterator now invokes a private recursive Serde
deserializer before creating a `serde_json::Value`. A JSON object containing
a repeated name fails with `JsonlError::Parse` rather than silently
overwriting one value. Detection happens at **every nesting depth**,
including objects inside arrays. JSON string escape sequences are decoded
before comparing names: `"key"` and `"k\\u0065y"` are duplicates, as are
literal Unicode keys and their escaped equivalents.

A repeated name in *different* objects is valid. Accepted JSON objects,
arrays, scalars and nulls retain their previous result types and value
semantics. The existing record byte cap, UTF-8 validation, single-JSON-text
requirement and terminal failure behavior remain in place.

## Guarantees and limits

- The parser raises an error before reading the value associated with the
  second occurrence. It does not echo the potentially sensitive duplicate
  name in the error message.
- All errors terminate the `JsonlRecords` iterator, and propagate through
  the downstream `TypedJsonl` adapter as `TypedJsonlError::Source`.
- `BoundedLines` still enforces the input byte budget. A record larger than
  its configured budget fails on input reading before parsing.
- The Serde JSON deserializer retains its ordinary JSON recursion limits.
  This is **not** a JSON canonicalization, cryptographic signing profile,
  Unicode normalization policy, or OS-enforced CPU/memory bound.
- This changes only the trusted Rust-host JSONL reader. Other JSON entrypoints
  using `serde_json::from_str::<Value>` are not automatically hardened.
- No actor runtime, compiler syntax, VM, cloud stream or durable replay
  behavior is changed.

## Validation

```bash
cargo test --no-default-features --test stream_json_duplicates
cargo test --no-default-features --test stream_json_schema
cargo test --no-default-features --test stream_records
cargo check --no-default-features
cargo fmt --all -- --check
```

Require normal exact-head CI and security audit. The Wasmtime advisory fix in
[#2068](https://github.com/nulang-org/nulang/pull/2068) is an independent
prerequisite for an audit-clean dependency graph: do not suppress audit
findings in the streaming PRs to hide it.
