# Nulang Error Codes

Every compiler/runtime diagnostic (`NuError`) carries a **stable error code**
shown in report headers as `[E0201] Error: ...` (ariadne reports) and
`error[E0201]: ...` (the `format_rich` fallback). Codes are category-scoped so
tooling can filter on the leading digits, and they are stable across releases:
a code, once assigned, never changes meaning.

Codes are returned by `NuError::stable_code()` (src/diagnostic.rs) and
rendered by both the ariadne report renderer (source snippets, carets) and
the hand-rolled `NuError::format_rich` fallback.

## Numbering scheme

| Range   | Category                                  |
|---------|-------------------------------------------|
| `E01xx` | Lexing & parsing                          |
| `E02xx` | Type checking (Hindley-Milner inference)  |
| `E03xx` | Algebraic effects                         |
| `E04xx` | Reference capabilities                    |
| `E05xx` | Runtime / VM                              |
| `E06xx` | Foreign interfaces (C FFI, Python interop)|
| `E09xx` | Miscellaneous (NYI features, packaging)   |

`NuError::Suspended` and `NuError::Multiple` carry no code of their own:
suspension is not an error, and each child of `Multiple` has its own code.

## Code table

| Code    | Meaning                                   | Variant(s)         |
|---------|-------------------------------------------|--------------------|
| `E0101` | Lex error (generic)                       | `LexError`         |
| `E0102` | Parse error (generic)                     | `ParseError`       |
| `E0103` | Unclosed delimiter                        | `ParseError`       |
| `E0200` | Type error (generic)                      | `TypeError`        |
| `E0201` | Type mismatch (unification failure)       | `TypeError`        |
| `E0202` | Unbound variable                          | `TypeError`        |
| `E0203` | Infinite type (occurs check failed)       | `TypeError`        |
| `E0204` | Record field not found                    | `TypeError`        |
| `E0205` | Wrong number of arguments                 | `TypeError`        |
| `E0206` | Match expression with no arms             | `ParseError`/`TypeError` |
| `E0208` | Capability-qualified type at FFI boundary | `TypeError`        |
| `E0300` | Effect error (generic)                    | `EffectError`      |
| `E0301` | Missing effect in declared effect row     | `EffectError`      |
| `E0302` | Unhandled effect (no handler installed)   | `EffectError`/runtime |
| `E0300`* | Spawn-time capability denied (message prefix `capability denied:`, names the missing `Net::TcpOut("host:port")` token; see SPEC2 §5.9) | `EffectError`/runtime |
| `E0400` | Capability error (generic)                | `CapError`         |
| `E0401` | Sendability violation (non-`val` cross-actor send) | `CapError` |
| `E0402` | Linear value used after consume           | `CapError`         |
| `E0501` | Runtime error (generic)                   | `RuntimeError`     |
| `E0502` | VM error (generic)                        | `VMError`          |
| `E0503` | VM step limit exceeded                    | `VMError`          |
| `E0601` | C FFI error                               | `FFIError`         |
| `E0602` | Python interop error                      | `PythonError`      |
| `E0901` | Feature not yet implemented               | `NotYetImplemented`|
| `E0902` | Package manager error                     | `PackageError`     |

Fine-grained codes (`E0103`, `E0201`–`E0206`, `E0208`, `E0301`/`E0302`,
`E0401`/`E0402`, `E0503`) are selected via `NuError::error_code()`, which
prefers structured payload fields (expected/found types, similar-name
suggestions, missing effects, capability explanations) over message-pattern
heuristics. Everything else falls back to the per-variant generic code
(`Ex0xx` with `00`/`01`/`02` suffix).

## Legacy flat codes and `--explain`

The original flat scheme (`E001`–`E013`, enum `ErrorCode` in src/types.rs)
remains for backwards compatibility. `nulang --explain <CODE>` accepts both
schemes:

```sh
nulang --explain E003    # legacy
nulang --explain E0201   # stable category-scoped (same diagnostic)
```

| Legacy | Stable  |
|--------|---------|
| `E001` | `E0103` |
| `E002` | `E0202` |
| `E003` | `E0201` |
| `E004` | `E0301` |
| `E005` | `E0401` |
| `E006` | `E0402` |
| `E007` | `E0203` |
| `E008` | `E0204` |
| `E009` | `E0205` |
| `E010` | `E0206` |
| `E011` | `E0503` |
| `E012` | `E0302` |
| `E013` | `E0208` |

## Output modes

- **tty**: ariadne report — `[Exxxx] Error: msg` header, `╭─[file:line:col]`
  source snippet with carets, notes for structured fields (expected/found
  types, missing effects, capability rule explanations), and a `Help:` line
  when a fix suggestion is known.
- **non-tty / CI**: plain `Display` output (`Error: Type error at L:C: msg`)
  is preserved unchanged so existing tooling and the conformance suite keep
  matching on stable prefixes. If no source map is available (pre-lexing
  errors, synthetic spans), the tty path falls back to
  `NuError::format_rich`.
