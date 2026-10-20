# Experimental CSV and JSONL record readers

This Rust-host implementation extends the `stream_pull` reference from
[#2065](https://github.com/nulang-org/nulang/pull/2065). It does not introduce
Nulang syntax or change VM, actor, WASM, or durable replay behavior.

`nulang::stream_records` provides two **pull-driven**, fail-closed iterators:

- `JsonlRecords<R: BufRead>`: one JSON value per line, including objects,
  arrays, and scalars. Empty lines and invalid JSON fail rather than being
  silently skipped. A line is bounded in raw UTF-8 bytes, including its
  newline delimiter, by the existing `BoundedLines` reader. JSON parser
  errors preserve their source type.
- `CsvRecords<R: BufRead>`: comma-delimited UTF-8 rows, with double-quoted
  fields, escaped double quotes, and embedded CR/LF within quoted fields.
  LF and CRLF terminate records outside quotes. Unclosed or misplaced quotes,
  bare CR outside quotes, invalid UTF-8, excessive field counts, and oversize
  records terminate reading with an error.

Both readers enforce a positive per-record raw-byte budget up to 16 MiB. CSV
also requires a positive column limit up to 4096. The CSV limit includes
commas, quote delimiters, and line endings, not just decoded field data.
A single physical file may be arbitrarily large; readers do not load it all.

## Examples

```rust
use nulang::stream_pull::PullStream;
use nulang::stream_records::{CsvRecords, JsonlRecords};
use std::{fs::File, io::BufReader};

let file = BufReader::new(File::open("events.jsonl")?);
let events = PullStream::new(JsonlRecords::new(file, 256 * 1024)?)
    .filter_ok(|value| value["kind"] == "error")
    .take(100)
    .try_collect_bounded(100)?;

let file = BufReader::new(File::open("data.csv")?);
let rows = PullStream::new(CsvRecords::new(file, 256 * 1024, 256)?)
    .take(100)
    .try_collect_bounded(100)?;
```

## Semantics and limitations

- Both iterators stop at their first error and cannot automatically resync
  at a later record; callers receive the underlying error.
- CSV returns `Vec<String>` for each row, *not* parsed numeric or header-based
  values; header interpretation belongs to a separate layer. Different row
  lengths are valid when within the configured field limit.
- An empty file has zero CSV records. An explicit blank CSV line yields
  a one-field empty row; a blank JSONL line is invalid.
- This CSV dialect is intentionally strict about garbage after a quoted
  field. It does not attempt delimiter autodetection, comment handling,
  relaxed quoting, BOM removal, or arbitrary dialect inference.
- Structured `serde_json::Value` materializes each JSON record; individual
  records are byte-bounded but total collection allocations depend on chosen
  item-count limits and decoded data. JSON object keys are checked for duplicates before `serde_json::Value` is constructed.
- Pull-driven operation is synchronous, not an asynchronous channel or
  scheduler-level backpressure system. No actor execution is enabled.

## Validation required

```bash
cargo test --no-default-features --test stream_records
cargo test --no-default-features --test stream_pull
cargo check --no-default-features
cargo fmt --all -- --check
```

The dependent PR #2065 must land first. The recommended next step after this
slice is a typed schema/header layer with explicit row validation and bounded
byte/item aggregation, not more syntax.
