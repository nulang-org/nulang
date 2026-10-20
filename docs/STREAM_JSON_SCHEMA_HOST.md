# Strict typed JSONL object rows (experimental Rust-host adapter)

This is an additive host-side stage on top of:
1. [#2065](https://github.com/nulang-org/nulang/pull/2065) pull streams.
2. [#2067](https://github.com/nulang-org/nulang/pull/2067) incremental CSV and JSONL record readers.
3. [#2072](https://github.com/nulang-org/nulang/pull/2072) typed CSV schema and bounded collection.

`nulang::stream_json_schema` validates *decoded* JSONL values as strictly
typed objects. It does not change the Nulang parser, VM, bytecode, scheduler,
actor effects, distributed transport, or durable workflow replay.

## Contract

`JsonSchema::new(Vec<JsonField>)` accepts 1–1024 unique, case-sensitive,
non-blank field names, at most 256 UTF-8 bytes per name. Each field uses the
existing `ColumnType`: `Text` (JSON string), `Integer` (signed 64-bit
JSON integer), or `Boolean` (JSON boolean).

`TypedJsonl::new(jsonl_records, schema)` is lazy and emits `JsonRow`
with field order matching the schema. Every configured field is required;
extra fields, nulls, arrays, numeric strings, non-integral JSON numbers,
unsigned integers outside i64, and type coercions are rejected. A failure
terminates the iterator instead of skipping invalid records or returning
partial objects. Errors identify JSONL record number and schema column
indices but avoid echoing record values into diagnostics.

The underlying `JsonlRecords` remains responsible for raw UTF-8 line
limits (up to 16 MiB), JSON syntax, and streaming one record at a time.

## Sample

```rust
use nulang::stream_budget::{collect_budgeted, CollectionBudget};
use nulang::stream_json_schema::{JsonField, JsonSchema, TypedJsonl};
use nulang::stream_records::JsonlRecords;
use nulang::stream_schema::ColumnType;
use std::{fs::File, io::BufReader};

let schema = JsonSchema::new(vec![
    JsonField::new("id", ColumnType::Integer),
    JsonField::new("name", ColumnType::Text),
    JsonField::new("active", ColumnType::Boolean),
])?;
let input = BufReader::new(File::open("data.jsonl")?);
let source = JsonlRecords::new(input, 128 * 1024)?;
let typed = TypedJsonl::new(source, schema);
let rows = collect_budgeted(typed, CollectionBudget::new(1000, 1024 * 1024)?, |row| {
    row.payload_bytes()
})?;
```

## Explicit limitations

- **Duplicate JSON keys are not rejected.** `JsonlRecords` currently
  deserializes with `serde_json::Value`, which does not retain duplicate
  occurrences. A future parsing stage must detect duplicate input keys
  *before* converting the record into `Value`. Do not use this adapter as a
  cryptographic canonicalization or security-policy parser.
- The `payload_bytes()` measurement accounts for UTF-8 key/value lengths
  plus simple scalar widths. It excludes JSON escaping, allocation capacity,
  allocator metadata and peak RSS. It is a **logical payload estimate**,
  not an OS-enforced memory cap.
- No nested objects, arrays, nullable or optional fields, floats, schema
  inference, unknown-field tolerance, or versioned schema migrations.
- Iteration is synchronous and pull-driven. There is no asynchronous
  backpressure, cancellation, durable exactly-once delivery or actor
  scheduling guarantee.
- Source errors are propagated through `TypedJsonlError::Source`; schema
  violations use `TypedJsonlError::Schema`.

## Required promotion checks

```bash
cargo test --no-default-features --test stream_json_schema
cargo test --no-default-features --test stream_schema_budget
cargo test --no-default-features --test stream_records
cargo test --no-default-features --test stream_pull
cargo check --no-default-features
cargo fmt --all -- --check
```

Keep draft until the three prerequisite PRs are validated/promoted and this
head passes all relevant checks. A sensible next increment is strict
duplicate-key detection before deserialization and fixed serialized-byte
budgets, followed by comprehensive parser+VM conformance before exposing
language-level typed streams.
