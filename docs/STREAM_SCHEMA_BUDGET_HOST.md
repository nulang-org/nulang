# Experimental typed CSV records and aggregate payload budgets

This experimental **Rust-host** slice extends the streaming stack from
[#2065](https://github.com/nulang-org/nulang/pull/2065) and
[#2067](https://github.com/nulang-org/nulang/pull/2067).
It does not modify Nulang syntax, effect dispatch, the VM, actors, or replay.

## Typed CSV decoding

`CsvSchema::new(Vec<Column>)` requires between 1 and 4096 columns with
nonempty, unique, case-sensitive names. Each column specifies one strict kind:
`ColumnType::Text`, `ColumnType::Integer` (signed 64-bit),
or `ColumnType::Boolean` (exact lowercase `true`/`false`).

`TypedCsv::new(csv_records, schema, expect_header)` reads from the incremental
CSV reader, optionally verifies that the first row contains **exactly** the
configured header names in order, then emits `CsvRow { cells: Vec<Scalar> }`.
The first row after the optional header is data row 1.

Wrong field counts, malformed integer/boolean values, missing/mismatched
headers, and upstream reader I/O errors are terminal. Errors identify the
row and column when applicable. They intentionally do not echo rejected cell
contents into error messages.

There are no implicit nulls, whitespace stripping, locale-specific numeric
decoding, header autodetection, field defaults, schema inference, or row
repair. The raw `CsvRecords` reader remains usable for untyped rows.

## Aggregate budgets

`CollectionBudget::new(max_items, max_payload_bytes)` requires two positive
limits, capped at 1,000,000 items and 64 MiB declared payload bytes.

`collect_budgeted(iter, limits, measure_payload_bytes)` takes a fallible
iterator, calls the measurement callback for each item, and errors if the
next item would exceed either limit. Integer overflow is treated as a byte
limit violation. On failure, the partial `Vec` is discarded; the returned
error distinguishes source failure, item-count overflow, and byte overflow.
Detecting a first excess item necessarily reads that item, but never reads
later items.

`CsvRow::payload_bytes()` adds UTF-8 string lengths plus a primitive
representation estimate (8 bytes per integer, 1 per boolean).

**Important:** These are *declared domain payload* budgets, **not** memory
allocator, stack, queue, overhead, or peak RSS budgets. The caller controls
the measuring function and could undercount. The CSV row method excludes
`Vec`, `String` capacity, struct metadata, and source buffers. Large
primitive object graphs, background tasks, or hostile custom iterators still
require process-level OS resource isolation.

## Example

```rust
use nulang::stream_budget::{collect_budgeted, CollectionBudget};
use nulang::stream_schema::{Column, ColumnType, CsvSchema, TypedCsv};
use nulang::stream_records::CsvRecords;
use std::{fs::File, io::BufReader};

let schema = CsvSchema::new(vec![
    Column::new("id", ColumnType::Integer),
    Column::new("name", ColumnType::Text),
    Column::new("active", ColumnType::Boolean),
])?;
let source = BufReader::new(File::open("people.csv")?);
let records = CsvRecords::new(source, 64 * 1024, 3)?;
let rows = TypedCsv::new(records, schema, true);
let collected = collect_budgeted(rows, CollectionBudget::new(1000, 1024 * 1024)?, |row| {
    row.payload_bytes()
})?;
```

## Validation

```bash
cargo test --no-default-features --test stream_schema_budget
cargo test --no-default-features --test stream_records
cargo test --no-default-features --test stream_pull
cargo check --no-default-features
cargo fmt --all -- --check
```

The schema/budget PR must remain draft until the two parent PRs land or are
otherwise fully validated, and until these checks and standard repository CI
pass. Next work: typed JSONL object-field schemas, aggregate serialized-byte
measurement, diagnostics and independent backend conformance tests.
