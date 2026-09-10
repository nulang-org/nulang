#!/usr/bin/env python3
from pathlib import Path


def replace_once(path: str, old: str, new: str) -> None:
    p = Path(path)
    text = p.read_text()
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{path}: expected exactly one match, found {count}: {old[:80]!r}")
    p.write_text(text.replace(old, new, 1))


replace_once(
    "src/jit/runtime.rs",
    '''/// Record an arithmetic runtime error for the AOT driver and yield nil
/// (compiled code cannot unwind; see `AOT_PENDING_ERROR`).
fn record_arith_error(e: crate::types::NuError) -> u64 {
    let msg = match e {
        crate::types::NuError::RuntimeError { msg, .. } => msg,
        other => other.to_string(),
    };
    aot_set_pending_error(msg);
    Value::nil().as_raw()
}''',
    '''/// Record an arithmetic runtime error for native compiled code and yield nil.
///
/// The helpers are shared by tiered JIT and standalone AOT, so route the
/// error to the currently-active execution channel instead of contaminating
/// the other backend's thread-local slot.
fn record_arith_error(e: crate::types::NuError) -> u64 {
    let msg = match e {
        crate::types::NuError::RuntimeError { msg, .. } => msg,
        other => other.to_string(),
    };
    if get_jit_vm().is_null() {
        aot_set_pending_error(msg);
    } else {
        set_jit_pending_vm_error(msg);
    }
    Value::nil().as_raw()
}''',
)

replace_once(
    "src/jit/runtime.rs",
    '''pub fn aot_set_pending_error(msg: String) {
    AOT_PENDING_ERROR.with(|e| {
        *e.borrow_mut() = Some(msg);
    });
}''',
    '''pub fn aot_set_pending_error(msg: String) {
    AOT_PENDING_ERROR.with(|e| {
        let mut slot = e.borrow_mut();
        if slot.is_none() {
            *slot = Some(msg);
        }
    });
}''',
)

replace_once(
    "src/jit/runtime.rs",
    '''pub fn set_jit_pending_vm_error(msg: String) {
    JIT_PENDING_VM_ERROR.with(|e| *e.borrow_mut() = Some(msg));
}''',
    '''pub fn set_jit_pending_vm_error(msg: String) {
    JIT_PENDING_VM_ERROR.with(|e| {
        let mut slot = e.borrow_mut();
        if slot.is_none() {
            *slot = Some(msg);
        }
    });
}''',
)

replace_once(
    "src/jit/runtime.rs",
    '''    let pending_error = AOT_PENDING_ERROR.with(|e| e.borrow().clone());''',
    '''    // Re-entrant interpreter calls are nested inside a tiered-JIT region.
    // Preserve the JIT channel, not the unrelated standalone-AOT channel.
    let pending_error = JIT_PENDING_VM_ERROR.with(|e| e.borrow().clone());''',
)

replace_once(
    "src/jit/runtime.rs",
    '''    AOT_PENDING_ERROR.with(|e| *e.borrow_mut() = s.pending_error);''',
    '''    JIT_PENDING_VM_ERROR.with(|e| *e.borrow_mut() = s.pending_error);''',
)

replace_once(
    "src/aot/mod.rs",
    '''        // Set up standalone heap for AOT runtime helpers.
        let mut heap = crate::runtime::heap::ActorHeap::new(1024 * 1024);''',
    '''        // A previous interrupted/older native run must never contaminate
        // this invocation.
        let _ = crate::jit::runtime::aot_take_pending_error();

        // Set up standalone heap for AOT runtime helpers.
        let mut heap = crate::runtime::heap::ActorHeap::new(1024 * 1024);''',
)

replace_once(
    "src/aot/codegen.rs",
    '''                } else {
                    let payload = emit_sext48(builder, val);
                    let neg = builder.ins().ineg(payload);
                    Ok(emit_tag_int(builder, neg))
                }
            } else if type_meta.is_known(reg as usize, KnownType::Float) {''',
    '''                } else {
                    // A statically-Int expression can still produce `nil` at
                    // runtime (negative integer exponent, div/mod by zero).
                    // Boxed mode must retain the runtime tag check.
                    call_helper(builder, helpers, "nulang_ineg", &[val])
                }
            } else if type_meta.is_known(reg as usize, KnownType::Float) {''',
)

replace_once(
    "src/jit/typed_compiler.rs",
    '''/// Opcodes the typed compiler knows how to emit.
///
/// This is deliberately a subset of `compiler::is_opcode_compilable`: the
/// typed compiler's catch-all arm jumps to the return block, so an
/// unsupported opcode in the middle of a region would silently drop the
/// remaining instructions. Callers must pre-check regions with this
/// function (as `compile_bytecode_region_typed` does) and fall back to the
/// scalar compiler for anything outside the set.
pub fn is_opcode_supported_typed(op: OpCode) -> bool {''',
    '''/// Opcodes the typed compiler knows how to emit soundly.
///
/// This is deliberately a subset of `compiler::is_opcode_compilable`: the
/// typed compiler's catch-all arm jumps to the return block, so an
/// unsupported opcode in the middle of a region would silently drop the
/// remaining instructions. Callers must pre-check regions with this
/// function (as `compile_bytecode_region_typed` does) and fall back to the
/// scalar compiler for anything outside the set.
///
/// Control flow is intentionally excluded for now. `infer_reg_types` computes
/// a per-PC CFG fixpoint, but typed codegen currently evolves one metadata
/// value in linear bytecode order. Those facts are not sound at joins or loop
/// backedges. Until codegen consumes per-PC metadata, control-flow regions
/// must use the scalar JIT.
pub fn is_opcode_supported_typed(op: OpCode) -> bool {''',
)

replace_once(
    "src/jit/typed_compiler.rs",
    '''            | OpCode::Jmp
            | OpCode::JmpT
            | OpCode::JmpF
''',
    "",
)

regressions = r'''
    /// Regressions minimized from the 2026-09-08 nightly campaign.
    #[test]
    fn differential_native_arithmetic_error_regressions() {
        let cases = [
            "-3 ** -2",
            "-(1 % 0)",
            "-fn(x) { x + 1 }",
            "--(1 + 2,)",
            "fn pick(b) { if b then 1 else -1 }; pick(true) * pick(-false)",
        ];

        for source in cases {
            if let Err(msg) = differential_fuzz_one(source) {
                panic!("native arithmetic regression on {source:?}: {msg}");
            }
        }

        differential_fuzz_one("0")
            .unwrap_or_else(|msg| panic!("native error-slot contamination: {msg}"));
    }

    #[test]
    fn differential_jit_control_flow_regressions() {
        let cases = [
            r#"let i2 = { var fa0 = 0
for x1 in [964593] { fa0 = fa0 + x1 }
fa0 }
i2"#,
            "var i = 0.0; var s = 0.0; while i < 100.0 { s = s + i * 2.5; i = i + 1.0; }; s",
        ];

        for source in cases {
            if let Err(msg) = differential_fuzz_one(source) {
                panic!("JIT control-flow regression on {source:?}: {msg}");
            }
        }
    }

'''
replace_once(
    "src/fuzz.rs",
    '''    /// Extended differential fuzz (ignored by default — run explicitly or
''',
    regressions + '''    /// Extended differential fuzz (ignored by default — run explicitly or
''',
)

replace_once(
    "docs/PERFORMANCE_ANALYSIS.md",
    '''### Typed path (`src/jit/typed_compiler.rs`)
''',
    '''### Typed path (`src/jit/typed_compiler.rs`)

**Correctness guard (2026-09-10):** typed guard stripping is restricted to
straight-line regions. Regions containing `Jmp`/`JmpT`/`JmpF` fall back to the
scalar JIT because current typed codegen evolves one `TypeMetadata` value in
lexical bytecode order; that is not a sound substitute for per-PC CFG facts at
backedges and branch joins. Restore typed control-flow compilation only after
codegen consumes per-PC metadata and the differential regressions remain green.

''',
)

replace_once(
    "CHANGELOG.md",
    '''*Breaking changes require an accepted RFC and a deprecation cycle of at least
two major versions.*

''',
    '''*Breaking changes require an accepted RFC and a deprecation cycle of at least
two major versions.*

### Fixed since 1.0.0-frozen — 2026-09-10

- **Native backend arithmetic-error parity** (`src/jit/runtime.rs`,
  `src/aot/mod.rs`, `src/aot/codegen.rs`). Tiered JIT arithmetic helpers route
  faults through the JIT pending-error channel rather than contaminating AOT;
  first error wins in both native channels; AOT clears stale error state at
  each run boundary; and boxed statically-Int negation retains runtime tag
  checking for nil-producing integer operations.
- **Tiered-JIT control-flow type-fact soundness** (`src/jit/typed_compiler.rs`).
  Typed guard stripping no longer compiles branches/backedges with one
  linearly-evolved metadata state. Control-flow regions fall back to scalar JIT
  until typed codegen consumes per-PC CFG analysis.

''',
)

replace_once(
    ".github/workflows/fuzz-nightly.yml",
    '''  workflow_dispatch: {} # allow manual runs for investigation

env:
  CARGO_TERM_COLOR: always
''',
    '''  workflow_dispatch: {} # allow manual runs for investigation

permissions:
  contents: read

concurrency:
  group: fuzz-nightly
  cancel-in-progress: true

env:
  CARGO_TERM_COLOR: always
  RUST_BACKTRACE: "1"
''',
)
replace_once(
    ".github/workflows/fuzz-nightly.yml",
    '''    runs-on: ubuntu-latest
    strategy:
''',
    '''    runs-on: ubuntu-latest
    timeout-minutes: 120
    strategy:
''',
)
replace_once(
    ".github/workflows/fuzz-nightly.yml",
    '''      - uses: actions/checkout@v7''',
    '''      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7''',
)
replace_once(
    ".github/workflows/fuzz-nightly.yml",
    '''      - name: Install Rust
        uses: dtolnay/rust-toolchain@stable
''',
    '''      - name: Install Rust 1.95.0
        uses: dtolnay/rust-toolchain@46817827a5bfabe028bf34e1cce71fd40e2ff697 # 1.95.0
''',
)
replace_once(
    ".github/workflows/fuzz-nightly.yml",
    '''      - name: Cache cargo
        uses: Swatinem/rust-cache@v2
''',
    '''      - name: Cache cargo
        uses: Swatinem/rust-cache@6323deb102c322ba6fcbdcafc7e3dddab59af2b6 # v2.9.2
''',
)
replace_once(
    ".github/workflows/fuzz-nightly.yml",
    '''        run: cargo test --lib --features wasm-backend fuzz_differential_extended -- --ignored --nocapture
''',
    '''        shell: bash
        run: |
          set -o pipefail
          LOG="/tmp/nulang-fuzz-shard-${{ matrix.shard }}.log"
          cargo test --lib --features wasm-backend \
            fuzz_differential_extended -- --ignored --nocapture 2>&1 | tee "$LOG"
''',
)
replace_once(
    ".github/workflows/fuzz-nightly.yml",
    '''        uses: actions/upload-artifact@v7
        with:
          name: fuzz-divergence-shard-${{ matrix.shard }}
          path: |
            /tmp/*.log
          if-no-files-found: ignore
''',
    '''        uses: actions/upload-artifact@043fb46d1a93c77aae656e7c1c64a875d1fc6a0a # v7
        with:
          name: fuzz-divergence-shard-${{ matrix.shard }}
          path: /tmp/nulang-fuzz-shard-${{ matrix.shard }}.log
          if-no-files-found: error
''',
)

print("Sev-1 source transformation completed")
