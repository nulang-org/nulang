# RFC 0015 migration: `catch` / `fail` → `Result` + `?` + `T ! E`

RFC 0015 (Error-Model Consolidation) deprecates the legacy `catch` and
`fail` constructs. Phase 1 (current release) emits warnings only — code
keeps compiling and running identically. They become hard errors in v2.0.

| Construct | Warning | Replacement |
|-----------|---------|-------------|
| `catch expr fallback`, `catch expr { \| pat => body, ... }`, `expr catch fallback` | `W0101` | `match` on `Ok`/`Error` |
| `fail expr` | `W0102` | `return` |

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

When you don't need a local fallback, prefer propagating with `?` under
an error-type signature (`T ! E` is sugar for `Result[T, E]`):

```nulang
fn load(path: String) -> Config ! Error {
  let text = read_file(path)?   // propagates Error(e) to the caller
  parse_config(text)
}
```

## Migrating `fail` (W0102)

`fail e` has always been literal sugar for `return e` — it performs no
error wrapping of its own. Rename the keyword:

```nulang
// before (deprecated)
fn head(l: List[Int]) -> Int ! Error {
  if empty(l) { fail Error("empty list") }
  first(l)
}

// after
fn head(l: List[Int]) -> Int ! Error {
  if empty(l) { return Error("empty list") }
  first(l)
}
```

## Division by zero (Phase 2, not yet in effect)

RFC 0015 also changes division semantics — integer div/mod by zero will
become a runtime fault and float div/mod will follow IEEE 754 — but that
is Phase 2 and lands separately. No behavior change ships with the
Phase 1 warnings.
