# Experimental source-language finite pull reference

The `stdlib::finite_stream` module provides a deliberately small
**Nulang-language** reference implementation on top of array and generic
ADT support. This is a separate experiment from the Rust-host
`PullStream` / CSV / JSONL PR stack. It is not the final opaque
`std.stream.Stream[T, E]` proposal and is **not** an actor stream.

Import explicitly:

```nula
import stdlib::finite_stream

fn main() {
    let cursor: FiniteCursor[Int] = from_items([10, 20])
    match advance(cursor) {
        | Produced(value, rest) => {
            perform IO.print(perform Int.to_string(value))
        }
        | Exhausted => perform IO.print("empty")
    }
}
```

## Available surface

- `FiniteCursor[T]`: an experimental record with `items: [T]` and
  `offset: Int`. It begins at offset zero.
- `FiniteStep[T]`: `Exhausted | Produced((T, FiniteCursor[T]))`.
- `from_items[T]([T]) -> FiniteCursor[T]` creates a cursor.
- `advance[T](FiniteCursor[T]) -> FiniteStep[T]` requests only the
  currently indexed item and returns the next cursor, or finishes.

The standard `Array.length` in-memory primitive is invoked to enforce
bounds on every pull. This is **not an IO-purity proof** in the effect
checker. There is no eager map/filter or hidden prefetch queue here.

## What the reference intentionally does not guarantee

**Cursor ownership is not enforced.** `FiniteCursor` is an ordinary
copyable Nulang record. Calling `advance` twice on the old value returns
the same item twice. It is therefore not a single-owner resource and is
not safe to expose as an exactly-once transport, durable receipt, or
capability. The authoritative `Stream` API must use an opaque handle
with proven affine/linear semantics before such guarantees are made.

The module is finite and memory-backed. It does not support network,
filesystem, subprocesses, cancellation, cleanup callbacks, asynchronous
backpressure, faults, typed `Result`/error rows or durable replay.
`from_items` keeps an array in the cursor; it does not bound total
input size or claim allocator/RSS protection.

The unqualified `Stream[T]` type name remains available to user source
code. Importing this module introduces distinct
`FiniteCursor` / `FiniteStep` names, rather than reserving `Stream`.

## Compatibility and validation

This change is stacked on [#2075](https://github.com/nulang-org/nulang/pull/2075),
which introduces RFC 0026 and protects the existing user-defined
`Stream[T]` behavior. This child adds behavioral cases:

- `stream_02_finite_cursor_pull`: two demand steps, then exhaustion.
- `stream_03_finite_cursor_empty`: immediate exhaustion on empty input.
- `stream_04_finite_cursor_names`: generic String cursor alongside
  a local `Stream[T]` recursive ADT.

Commands to execute in CI:

```bash
python3 scripts/generate_stdlib.py --check
python3 conformance/run.py
cargo fmt --all -- --check
cargo test --locked --lib
```

These tests must be run by the actual typechecker + interpreter before
promotion. A separate compiler/VM capability suite must establish matching
behavior across other execution backends. This branch is intentionally
draft until those results exist.

Next increments: prove exact ownership with a typed opaque cursor,
backed by an explicit `Next` effect or VM handle; then add fallible
transformations and cancellation without hidden eager materialization.
