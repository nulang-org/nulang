# Typed deterministic test-input generation (experimental)

This is an opt-in Rust-level testing API in `nulang::dst::generate`. It is
**not** a new `.nula` language construct, source-level `spec` declaration,
production actor admission check, or replacement for existing DST.

## API

- `generate_value(&Type, &mut DeterministicRng, &GenerationLimits)` samples a
  supported closed type.
- `generate_message(&MessageSpec, &mut DeterministicRng, &GenerationLimits)`
  builds a `SimMessage` with appropriately shaped arguments.
- `check_property(&Type, seed, trials, &GenerationLimits, predicate)` evaluates
  a pure predicate on deterministic samples, returning the first counterexample
  with its seed and zero-based trial index.
- Existing `dst::shrink::minimize_failing_trace` minimizes a generated
  *sequence of messages* when the caller provides a deterministic failure
  predicate replaying the sequence on a fresh simulator.

## Example

```rust
use nulang::dst::{DeterministicRng, SimValue};
use nulang::dst::generate::{check_property, generate_message, GenerationLimits, MessageSpec};
use nulang::types::{PrimitiveType, Type};

let ty = Type::Primitive(PrimitiveType::Int);
let first_failure = check_property(&ty, 42, 100, &GenerationLimits::default(), |value| {
    matches!(value, SimValue::Int(_))
})?;
assert!(first_failure.is_none());

let spec = MessageSpec {
    sender: 0,
    target: 1,
    behavior: "credit".into(),
    parameters: vec![ty],
};
let mut rng = DeterministicRng::new(42);
let test_message = generate_message(&spec, &mut rng, &GenerationLimits::default())?;
assert_eq!(test_message.payload.len(), 1);
# Ok::<(), Box<dyn std::error::Error>>(())
```

## Semantics and safety

- Supported: `Int`, `Float`, `Bool`, `String`, `Nil`, `Unit`,
  tuples, closed records, variants and arrays.
- Rejected: `Never`, `Address`, references, unresolved variables, open
  records, nominals, functions, actors and generic applications. These need
  additional runtime or compiler witnesses before they can be generated
  without violating type semantics.
- Record fields and variant tags are traversed in canonical lexical order,
  independent of source declaration order.
- The generator has an explicit recursion limit, bounded collection and
  string lengths, and a conservative maximum of 4,096 generated value nodes.
  Oversized shapes are rejected *before* RNG consumption or allocation.
- Generated strings use ASCII alphanumerics; generated floats are finite.
  Thus this is not yet an exhaustive Unicode or IEEE-754 edge-case generator.
- The generic `SimValue` representations are not the production VM `Value`
  representations; this feature does not change the bytecode, wire format,
  compiler language semantics or durable event storage.
- An invariant callback must be pure, or (for DST message traces) replay
  against a fresh deterministic simulator for every minimization attempt.
- A passing property check means only that the sampled cases passed. It is
  not a proof of the invariant.

## Next steps

1. Add compiler-produced testable protocol signatures with full type resolution.
2. Add shrinkers for individual scalar/record/variant values; the existing
   trace minimizer only removes **messages** from a sequence.
3. Expose optional typed contract declarations in the language only after
   the Rust-level generator and backend parity are stable.
4. Add a capability-authorized read-only actor introspection API separately.
