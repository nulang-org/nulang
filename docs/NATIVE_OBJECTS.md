# Native object artifacts

Nulang can emit relocatable native object files through the optional
`native-object` feature.

This path is intentionally separate from the executable `--backend native`
path:

- `--backend native` currently uses Cranelift `JITModule` and executes code
  in-process.
- `NativeObjectArtifact` uses Cranelift `ObjectModule` and returns
  relocatable object bytes for a later linker/runtime packaging step.
- Both paths use the same MIR-to-Cranelift lowering in `src/aot/codegen.rs`.
  Object emission does not maintain a second language-semantics implementation.

## Current artifact contract

`NativeObjectArtifact::compile(&mir, target)` produces:

- relocatable ELF/COFF/Mach-O bytes appropriate to the selected Cranelift ISA;
- a stable exported `nulang_entry` symbol when the MIR module contains
  `__main` or `main`;
- unresolved imports for Nulang runtime helpers such as arithmetic, heap,
  effect, closure, and FFI helpers;
- the exact native constant-pool metadata required by a later runtime/linker
  packaging layer.

Library modules without an entry function still emit valid objects and have no
`nulang_entry` export.

Actor modules currently fail closed. The next artifact milestone is to export
the existing stable `NativeActorEntry` wrappers and carry behavior metadata
into the link/runtime manifest; object emission must not invent a second actor
ABI.

## Rust API

```rust
let artifact = nulang::aot::object::NativeObjectArtifact::compile(&mir, "native")?;
artifact.write_to("module.o")?;
```

Build or test the feature directly:

```bash
cargo test --locked --no-default-features --features native-object native_object
```

## Non-goals of this slice

This does not yet link a standalone executable, embed the Nulang runtime,
serialize the constant/runtime manifest beside the object, or export actor
behavior entry points. Those are packaging/linking concerns layered on the
same native lowering after object generation is proven stable.
