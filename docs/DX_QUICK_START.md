# Everyday Nulang: one path from first program to advanced features

This guide uses **the existing language**. It does not introduce an alternative
runtime, a new effect model, or an implicit authority grant.

## First program (optional prelude)

```nula
import stdlib::prelude

fn main() {
    println("Hello from Nulang")
    let count = string_length("hello")
    println(int_to_string(count))
}
```

Run a standalone file with `nulang app.nula`. Check without running via
`nulang check app.nula` (equivalent to `nulang --check app.nula`).

The prelude is **opt in**; it provides a tiny set of ordinary functions:
- `println(message: String)` forwards to `perform IO.print(message)`.
- `string_length(value: String)` forwards to `perform String.length(value)`.
- `int_to_string(value: Int)` forwards to `perform Int.to_string(value)`.

The primitive operations remain available. The wrapper does not bypass effect
tracking, capability checks, or external-authority policy.

## Working with packages

```bash
nulang new example-app
cd example-app
nulang run
nulang test
nulang build
```

These commands call the same implementation as `nulang nula new`,
`nulang nula run`, `nulang nula test`, and `nulang nula build`.
Other direct aliases include `add`, `remove`, `list`, `clean`,
`watch`, `dev`, `doc`, `publish`, `deploy`, and `build-wasm`.

**Not an alias:** `nulang init <name>` still means legacy experiment
scaffolding; use `nulang nula init` to initialize a package in place.
To run a standalone source file, use `nulang file.nula` rather than
`nulang run file.nula` (the latter invokes the package runner).

## Idiomatic error handling

Use `Result` values and `match` for explicit recovery. Use `?`
where the enclosing function supports compatible error propagation.
Legacy `catch` and `fail` syntax is deprecated (RFC 0015) and is
kept for existing source compatibility, not as the preferred style.

## Learn only what the application needs

1. **Core:** local functions, collections, records, variants, `Result`.
2. **Concurrent:** scoped tasks for bounded parallel work; actors for
   independently addressable isolated state.
3. **Durable/distributed:** entities, workflow history, supervision,
   capability qualifiers, effect handlers, and explicit external authority.

These are learning stages, **not compiler modes**. Everything shares the
same type system and intermediate representation.

Nulang's reference bytecode VM has broader semantic coverage than
experimental WASM/native-AOT targets. Features such as durable workflows
must be validated against the selected backend; a convenience API is not
proof of backend parity. Refer to `docs/IMPLEMENTATION_STATUS.md`.

For advanced semantics, see `SPEC2.md` and
`RFC/0024-orthogonal-execution-model.md`.
