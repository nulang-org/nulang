# Machine-readable compiler fixes for coding agents

The `nulang --json --check path/to/source.nula` command emits one
`JsonReport` (schema version 1) on stdout. Logs and progress belong on
stderr. It exits unsuccessfully when the compiler reports an error.

## Text-edit contract

For a uniquely suggested in-scope name that exactly matches the primary error
span, the compiler may emit a `suggestion` like:

```json
{
  "message": "consider replacing `countr` with `counter`",
  "replacement": "counter",
  "edit": {
    "file": "program.nula",
    "start_byte": 28,
    "end_byte": 34,
    "expected_text": "countr",
    "new_text": "counter"
  },
  "applicability": "requires-review"
}
```

Offsets are zero-based, half-open **UTF-8 byte offsets**, not Unicode scalar
indices or LSP UTF-16 columns. Consumers should:

1. Treat compiler output as a suggestion, not an authorization to modify files.
2. Ensure the source file is the intended trusted workspace file; resolve paths
   against the workspace and reject escapes/symlinks outside allowed roots.
3. Read the current file, validate its identity/revision, and check that the
   byte slice `[start_byte, end_byte)` is exactly `expected_text`.
4. Apply a reviewed edit atomically; do not edit on source mismatch.
5. Re-run `--check` and focused tests before accepting the result.

Never use `replacement` alone to guess where to edit. The `edit` field is
optional and emitted only when source and error metadata agree. Ambiguous names,
stale spans, missing source maps, and advisory suggestions do not receive an
edit. Even exact matches do **not** establish that the candidate was intended;
the applicability value therefore remains `requires-review`.

This is an **additive schema-v1 extension**. Consumers of older reports should
continue to accept `suggestion: null` and suggestions without `edit`. Do not
treat missing fields as implicit permission to rewrite code.

## Validation

The focused Rust tests are under `src/json_diagnostics.rs`:

```sh
cargo test --locked json_diagnostics::tests
```

The tests cover uniquely suggested identifiers, ambiguous alternatives,
source mismatch and absence, and UTF-8 byte boundaries. The standard
repository test and lint gates remain mandatory before merging.

The human-facing diagnostic renderer and existing error-code assignments
remain unchanged. See [ERROR_CODES.md](ERROR_CODES.md) for stable codes.
