# RFC 0015 migration: consolidate on `Result` + `?` + effects + supervision

RFC 0015 (Error-Model Consolidation) deprecates the legacy `catch` and
`fail` constructs and typed-error signature shorthands. Phase 1 (current
release) emits warnings only — code keeps compiling and running identically.
They become hard errors in v2.0.

| Construct | Warning | Replacement |
|-----------|---------|-------------|
| `catch expr fallback`, `catch expr { \| pat => body, ... }`, `expr catch fallback` | `W0101` | `match` on `Ok`/`Error` |
| `fail expr` | `W0102` | `return` |
| `-> T ! E`, `-> T throws E` | `W0103` | `-> Result[T, E]` |

Warnings are printed to stderr with a source snippet. Pass
`--deny-warnings` to turn them into build errors (e.g. in CI, to prevent
new uses).

## Migrating `catch` (W0101)

`catch` is exactly sugar for a `match` on the `Result` variants — the
parser has always desugared it that way, so the rewrite is mechanical:

```nulang
// before (deprecated)
let port = catch parse_port(env) 8080

// after
let port = match parse_port(env) {
  | Ok(p) => p
  | Error(_) => 8080
}

// before (block form)
catch read_config(path) {
  | Error(msg) => default_config(msg)
}

// after
match read_config(path) {
  | Ok(c) => c
  | Error(msg) => default_config(msg)
}
```

When you don't need a local fallback, propagate with `?` from an explicit
`Result[T, E]` return type:

```nulang
fn load(path: String) -> Result[Config, Error] {
  let text = read_file(path)?   // propagates Error(e) to the caller
  parse_config(text)
}
```

## Migrating `fail` (W0102)

`fail e` has always been literal sugar for `return e` — it performs no
error wrapping of its own. Rename the keyword:

```nulang
// before (deprecated)
fn head(l: List[Int]) -> Result[Int, Error] {
  if empty(l) { fail Error("empty list") }
  first(l)
}

// after
fn head(l: List[Int]) -> Result[Int, Error] {
  if empty(l) { return Error("empty list") }
  first(l)
}
```

## Migrating typed-error signatures (W0103)

Typed-error syntax currently lowers to `Result[T, E]`, so migration is
mechanical and does not change runtime representation:

```nulang
// before (deprecated)
fn load(path: String) -> Config ! IOError ! {FS} {
  read_config(path)?
}

// also deprecated
fn load(path: String) -> Config throws IOError ! {FS} {
  read_config(path)?
}

// after
fn load(path: String) -> Result[Config, IOError] ! {FS} {
  read_config(path)?
}
```

This deliberately leaves `! {FS}` in one role only: algebraic-effect rows.
Recoverable errors are values (`Result`), while actor faults remain the
supervision layer.## Division by zero (Phase 2, not yet in effect)

RFC 0015 also changes division semantics — integer div/mod by zero will
become a runtime fault and float div/mod will follow IEEE 754 — but that
is Phase 2 and lands separately. No behavior change ships with the
Phase 1 warnings.
