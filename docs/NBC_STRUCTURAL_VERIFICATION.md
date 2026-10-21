# Opt-in NBC structural verification

Status: **experimental, opt-in**. Inspired by the JVM's bytecode verifier, but
**not** a full JVM-equivalent verifier or a sandbox boundary.

## Purpose and API

A decoded `.nbc` artifact can contain structurally invalid references, even
when the frozen header, opcode stream, and JSON metadata are well formed.
Legacy decoding remains unchanged for compatibility:

- `CodeModule::from_nbc(bytes)`: versioned decoder, **not** a verifier.
- `CodeModule::from_nbc_with_structural_verification(bytes)`: decode and check
  structural code references before returning the module.
- `verify_module_structure(&module)`: inspect an already decoded module.
- `CoreVM::load_nbc_with_structural_verification(bytes)`: reject an invalid
  module **before** inserting it into the CoreVM's module table.

The opt-in pass checks:

- entry point, named-function bytecode offsets, function-local metadata count;
- behavior and compensation code offsets;
- effect-handler binding and fallback code offsets;
- constants referencing function or behavior tables;
- direct and conditional branch targets, using the VM's PC-relative convention;
- `ConstU`, `Closure`, `Handle`, and `PerformDirect` table references;
- string-constant operands for `Perform`, `StateGet`, `StateSet`, `Emit`,
  and `SignalWait`.

Errors use the existing `FormatError::BodyDecode` variant with a
`structural verification:` prefix; there is **no** new stable format version,
opcode, metadata field, or wire-contract change.

## Security and compatibility limitations

The pass does **not** prove definite register initialization, register type
safety, effect-handler stack discipline, actor sendability, authority,
termination, deterministic replay, memory safety for arbitrary FFI, or
resource isolation. Some opcodes and complex metadata are not yet analyzed.
It is **not** permission to execute untrusted native bytecode, skip
runtime checks, or deploy an unaudited application server.

This pass is currently opt-in, so existing `.nbc` loader paths are **not**
hardened automatically. Use sandboxed WASM components and host-enforced
resource/authority policy for untrusted execution. Artifact input-size
admission is separate work (draft PR #1815); to avoid excessive allocations
in an exposed loader, size limits must be applied **before** decoding.

Structural verification is not artifact authentication: pinning and
verifying immutable artifact bytes is a separate requirement for durable
execution, currently being addressed in the workflow recovery stack.

## Validation

```sh
cargo fmt --all -- --check
cargo test --locked --test nbc_structural_verification
cargo test --locked
```

The initial RED specification is commit `de006d0`, followed by GREEN source
changes. Source-level RED is not evidence of an executed failing test.
At authorship, this environment had no Rust toolchain, so those commands
must be run on the exact PR head before promotion.

Next steps, **after** compatibility and performance validation: complete
opcode-specific operand checks, dataflow verification of initialized and
typed registers, effect-handler stack consistency, deterministic fuzzing
of malformed artifacts, and opt-in policy integration with bounded decoding.
Do not label the result a full bytecode verifier until those invariants
are enforced and independently tested.
