# Deterministic property testing (Rust DST)

The Rust-side `nulang::dst::property::check_trace_property` helper runs
QuickCheck-inspired properties over generated **operation traces**. It uses
Nulang's existing SplitMix64 `DeterministicRng` and
`dst::shrink::minimize_failing_trace`. This is **not yet** a source-language
`nula test` feature or a general automatic value shrinker.

## Contract

- `TracePropertyConfig { seed, cases, max_shrink_replays }` defines a bounded
  sweep. Each generated case receives a separate, recorded `case_seed`.
- `generate(&mut DeterministicRng) -> Vec<Op>` constructs an operation trace.
  Stateful protocol generators should emit **valid** sequences.
- `property_holds(&[Op]) -> bool` returns true if the invariant holds.
  **Recreate the Runtime and replay from clean state on every call**, including
  calls triggered by shrinking. Never reuse a mutated Runtime.
- The first failure returns `TracePropertyFailure` with root seed, case index,
  case seed, original events, and a minimized trace.
- Shrinking only **removes events while preserving order**. It does not shrink
  payload values. `OneMinimal` proves single-event deletion minimality for
  the supplied predicate, not the globally shortest counterexample.
- `NotFailing` means the first failing result could not be reproduced by the
  mandatory baseline replay. `BudgetExhausted` is explicitly incomplete
  evidence, including when the budget is zero.

## Example

```rust
use nulang::dst::property::{check_trace_property, TracePropertyConfig};

let result = check_trace_property(
    TracePropertyConfig {
        seed: 0x5eed,
        cases: 100,
        max_shrink_replays: 500,
    },
    |rng| vec![rng.next() % 16, rng.next() % 16],
    |values| {
        // Replace with a fresh real-runtime replay of the generated input.
        values.len() == 2
    },
);
assert_eq!(result, Ok(100));
```

## Validation

Focused tests:

```bash
cargo test --locked --lib dst::property_tests
cargo test --locked --lib runtime::cluster_dst_trace_tests
```

Full core validation:

```bash
cargo fmt --all -- --check
cargo test --locked --lib --no-default-features
cargo test --locked --lib
```

For CI and failure reports, print `case_seed`, the original trace, and
`minimized.status`; only cite the minimized trace as reproduced if the status
is `OneMinimal` or `BudgetExhausted` following at least one successful
baseline check. A shrink-budget exhaustion with zero replays does not establish
independent reproduction.

Recommended follow-ups: stateful generator combinators (valid crash/restart,
epoch fencing, message delivery), value shrinking, CRDT laws, persistence
round-trip properties, and differential bytecode/native/WASM properties. Keep
these separate from the general production runtime API.
