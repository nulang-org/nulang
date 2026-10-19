# numeric (Experimental)

An opt-in, zero-external-dependency Nulang package for **typed logical Float
matrices**. It is a correctness-first reference implementation using Nulang's
existing `[Float]` arrays, not a native packed buffer or optimized BLAS kernel.
Neither Nulang's language semantics nor its default runtime dependencies change.

## Run the tests

From `packages/numeric`:

```sh
nula test
```

If built from this repository, use the `nula` command from your Nulang
installation. This package is not automatically promoted to the official
registry; add from a local checkout until publication is authorized.

## API

| Function | Contract |
| --- | --- |
| `valid_shape(data, rows, cols) -> Bool` | Nonnegative dimensions and exact row-major element count; zero dimensions require empty data |
| `from_flat(data, rows, cols) -> Option[Matrix]` | Validated constructor for a `{rows, cols, data:[Float]}` record |
| `sum_values(values) -> Float` | Ordered left-to-right reduction; empty is `0.0` |
| `dot(a, b) -> Option[Float]` | Rejects mismatched lengths; two empty vectors have dot product `0.0` |
| `at(m, row, col) -> Option[Float]` | Bounds-checked row-major indexing; rejects forged malformed matrix records |
| `matmul(a, b) -> Option[Matrix]` | Reference matrix multiplication; rejects incompatible or malformed operands |

## Example

```nulang
import lib
import stdlib::test

fn main() {
    match from_flat([1.0, 2.0, 3.0, 4.0], 2, 2) {
        Some(a) => {
            match matmul(a, a) {
                Some(product) => assert(product.data[0] == 7.0, "top-left value"),
                None => fail_with("matmul should succeed"),
            }
        },
        None => fail_with("valid 2x2 data should construct"),
    }
}
```

## Limitations and next gate

- `Matrix` is a public structural record, not an opaque type. The validation
  function is intentionally repeated on every operation.
- `[Float]` uses existing VM values. This package does not supply packed
  `f32`/`f64` storage, slice views, zero-copy actor handoff, or hardware SIMD.
- `matmul` uses Nulang's `Array.push` API and has no BLAS acceleration;
  do not use this reference algorithm for high-throughput matrix workloads.
- Arithmetic follows Nulang's current Float semantics and operation order;
  no parallel reassociation or numerically stable summation is promised.
- Tests must pass via `nula test` on an actual compiler/runtime before
  moving this feature out of draft. The earlier numerical Criterion baselines
  must be certified separately, without inferring SIMD from a warm JIT run.

This small library establishes the API/correctness boundary before designing
a packed numeric buffer ABI or compiler-specialized vector loops.
