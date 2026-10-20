# Constrained record schema derivation (experimental)

Nulang has an existing record-derive syntax. This small Crystal-inspired
extension adds a **compiler-generated JSON Schema inspection helper** while
retaining the existing `@derive(eq)` behavior.

```nula
@derive(schema)
type Payment = { currency: String, amount: Int }

let definition = payment_schema()
```

The compiler synthesizes a **private, pure, zero-argument** function named
`<lowercase_record_name>_schema` returning a constant JSON string. The
result describes an object whose fields are all required. Nested closed
records and arrays of supported types are accepted. Schema properties and
`required` entries are sorted by field name for deterministic output.

### Contract and limitations

- Supported leaf types: `Int`, `Float`, `Bool`, `String`, `Unit`, `Nil`.
- Supported composites: closed records and arrays, recursively. Duplicate
  fields and open record tails are rejected.
- Generic records, references, functions, actors, variants, type variables,
  nominal wrappers, type applications, and other unresolved shapes are
  **rejected** rather than converted into permissive `{}` schemas.
- Unknown or duplicate derive names, helper-name collisions, and using
  `@derive` on non-record types cause compile-time errors.
- `@derive(eq, schema)` is supported. `@derive(eq)` continues to emit its
  existing structural comparison helper.
- There is no user-defined macro engine, AST plugin execution, or implicit
  authority grant. Schema helpers perform no I/O.
- **This does not derive JSON encoders or decoders.** It is not a durable
  wire-format contract, schema registry, or permission to reuse compiled
  artifacts. Changing a schema can still require migration/version review.

The existing `tool_schema::type_to_json_schema` converter is reused only
after validating the strict supported subset. That converter remains
best-effort for tool metadata and must not be used as a fail-closed durable
codec.

### Validation before promotion

```sh
cargo fmt --all -- --check
cargo test --locked --lib parser::tests::derive_
cargo test --locked --lib
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
```

Require exact-head GitHub CI and any applicable Woodpecker checks. Run
default-feature, no-default-feature and WASM builds before promotion.

This slice does not implement the broader typed derivation proposal.
A future RFC should specify public visibility, generic schemas, recursive
definitions, schema versioning, reference-capability constraints, and
backend equivalence before introducing codecs or wire integration.
