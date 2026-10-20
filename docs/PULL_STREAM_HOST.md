# Experimental pull streams for trusted Rust hosts

The `nulang::stream_pull` module is an independent, **host-side** starting
point for Nulang's proposed Nushell/AWK/Sed-style streaming APIs. It is not
the Nulang-language `Stream[T]` type and does **not** introduce actor,
cloud, workflow, or asynchronous scheduling semantics. It has no dependency
on the separate experimental dataflow graph PR.

## Scope

- `PullStream::new(iterator)`: wrap a fallible Rust iterator without
  eager materialization or prefetching.
- `map_ok`, `filter_ok`, and `take`: lazy composition; filtering never
  hides source errors; `take(n)` requests at most `n` output items.
- `try_fold_ok`: sequential fold that stops on the first source error.
- `try_collect_bounded(max_items)`: require a positive terminal item cap.
  It returns `LimitExceeded` if there are more items, rather than silently
  truncating. Detecting excess consumes one extra item.
- `BoundedLines<R: BufRead>`: incremental UTF-8 line records. It rejects
  invalid UTF-8, I/O errors, and lines larger than a configured raw byte
  limit (including LF/CRLF) before buffering beyond that limit. An error
  terminates the record iterator to avoid partial resynchronization.

The maximum line-record cap is 16 MiB. `BoundedLines` does not buffer an
entire file, but consumers must still configure a terminal item limit when
materializing a collection. This implementation uses **pull-driven demand**,
not asynchronous backpressure or bounded-channel scheduling.

## Example

```rust
use nulang::stream_pull::{BoundedLines, PullStream};
use std::io::BufReader;

let file = std::fs::File::open("app.log")?;
let lines = BoundedLines::new(BufReader::new(file), 64 * 1024)?;
let errors = PullStream::new(lines)
    .filter_ok(|line| line.contains("ERROR"))
    .map_ok(|line| line.to_uppercase())
    .take(100)
    .try_collect_bounded(100)?;

for line in errors {
    println!("{line}");
}
```

`try_collect_bounded(100)` on a stream already constrained with `take(100)`
will return at most 100 items. Without `take`, it deliberately errors if
there are 101 or more, rather than quietly discard records.

## Explicit non-goals

- No `Stream[T]` type, parser changes, opcode, or serialized ABI.
- No async operators, background production, thread pools, buffering queues,
  cancellation tokens, or cross-actor backpressure.
- No unlimited `collect()` convenience on `PullStream`.
- No transparent durable replay, distributed delivery, exactly-once
  processing, or arbitrary record framing.
- No strict bound on total bytes in a collected `Vec<T>`: the terminal cap
  counts items, so large arbitrary item types require an additional byte
  budget at the application's boundary.

## Validation

```bash
cargo test --no-default-features --test stream_pull
cargo check --no-default-features
cargo fmt --all -- --check
```

Before promotion, also run the repository's standard CI. Next slices can
add incremental CSV/JSONL record parsing and explicit byte budgets, then
bounded asynchronous channels with cancellation and actor admission policies.
