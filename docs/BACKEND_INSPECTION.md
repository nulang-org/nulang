# Backend compiler inspection

The `--inspect` CLI mode reports whether the selected backend **accepts and
compiles** a Nulang source file. It reuses the normal frontend and MIR, then
invokes the selected real code generator. It never executes generated program
code or writes a compiler artifact.

```sh
nulang --inspect --backend bytecode examples/fibonacci.nula
nulang --inspect --backend native examples/fibonacci.nula
# Requires a CLI built with --features wasm-backend:
nulang --inspect --backend wasm examples/fibonacci.nula
```

Inspection always writes one JSON object to stdout; optional frontend
warnings can go to stderr. The command returns exit status 0 **only** when
the selected compiler accepts the source. Invalid input, missing compiler
features and failed code generation return a nonzero status with structured
JSON. `--verbose` does not inject human-readable text into stdout in
inspection mode.

## v1 JSON contract

```json
{
  "schema_version": 1,
  "backend": "bytecode",
  "status": "compiles",
  "verification": "compile_only",
  "summary": {
    "functions": 1,
    "behaviors": 0,
    "foreign_functions": 0
  },
  "diagnostics": []
}
```

The example illustrates the schema; exact counts depend on the lowered
program. The version identifies the JSON interface, **not** the frozen
Nulang artifact format.

The `status` field is one of:

- `compiles`: the actual backend compiler accepted the MIR.
- `rejected`: frontend, MIR lowering, or backend compilation rejected it.
- `unavailable`: the backend name is unknown or was excluded at build time.

Diagnostic entries have stable `code` identifiers and a human-readable
`message`. Initially supported codes are `frontend.rejected`,
`backend.rejected`, `backend.disabled`, and `backend.unknown`.
Backend error messages can change; tooling should branch on `code`.

## Important limitations

**Compiler acceptance is not semantic or deployment certification.** A
successful report does not prove native/WASM parity with the reference VM,
successful module instantiation, presence of runtime host imports, authority
enforcement, actor scheduling, JIT tier-up, durable effect replay, or a valid
production deployment. The bytecode VM remains the reference semantics.

Native compilation currently builds code into Cranelift-managed JIT memory;
this is not proof that a portable object file exists. WASM inspection uses
the `wasm` compiler and does not instantiate a Wasmtime store or attempt
`wasm-aot` serialization.

No generated-program instructions execute during inspection. The compiler
itself may read imports and allocate memory as in a normal build, so it
should not be treated as a security sandbox for untrusted source code.

`--inspect` accepts one source file, with optional `--backend`,
`--target` (for native), `--with`, and `--deny-warnings`. It cannot be
combined with other execution or artifact-emission modes.

## Validation

```sh
cargo test --locked --test cli_backend_inspect
cargo test --locked backend_inspect
cargo test --locked --no-default-features backend_inspect
cargo test --locked --features wasm-backend backend_inspect
cargo fmt --all -- --check
```

The full repository CI, backend differential conformance, and
process-kill/replay tests remain separate requirements.
