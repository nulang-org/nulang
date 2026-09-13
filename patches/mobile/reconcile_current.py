#!/usr/bin/env python3
"""Apply the mobile runtime boundary to the current main baseline.

This wrapper keeps all exact-context reconciliation in Python so the GitHub
Actions workflow remains declarative and easy to validate. Every replacement
asserts its expected match count; drift fails closed.
"""

from pathlib import Path
import subprocess
import sys


def replace_once(path: str, old: str, new: str) -> None:
    p = Path(path)
    text = p.read_text()
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{path}: expected exactly one match, found {count}: {old[:100]!r}")
    p.write_text(text.replace(old, new, 1))


# Current main added scheduler_bench after the original boundary patch was
# authored. Preserve it in both native-codegen and interpreter-only benchmark
# registrations before applying the base transform.
patcher = Path("patches/mobile/apply_current.py")
text = patcher.read_text()
old = "    persist_bench::benches,\n"
new = "    persist_bench::benches,\n    scheduler_bench::benches,\n"
count = text.count(old)
if count != 3:
    raise SystemExit(f"apply_current.py: expected 3 benchmark-list anchors, found {count}")
patcher.write_text(text.replace(old, new))

subprocess.run([sys.executable, str(patcher)], check=True)

# The Wasm SIMD analyzer's only production consumer is the Wasm backend and it
# imports JIT-owned SIMD types. Keep it with that backend so interpreter-only
# mobile builds do not pull the JIT module tree into the source graph.
replace_once(
    "src/lib.rs",
    "pub mod mir_wasm_simd;",
    '#[cfg(feature = "wasm-backend")]\npub mod mir_wasm_simd;',
)

# TieredAction is only used by the native JIT path. The VM retains the stable
# JitBackend trait slot in interpreter builds, so no runtime API fork is needed.
replace_once(
    "src/vm.rs",
    "use crate::backends::{create_default_jit, JitBackend, TieredAction};",
    'use crate::backends::{create_default_jit, JitBackend};\n#[cfg(feature = "native-codegen")]\nuse crate::backends::TieredAction;',
)

# Integration tests expose an opt-in native backend through NU_BACKEND=native.
# Compile that arm only with native-codegen and reject it explicitly otherwise
# instead of silently falling back to bytecode.
replace_once(
    "src/integration_tests/mod.rs",
    '''        match backend() {
            "native" => {
                let aot_module = crate::aot::AotModule::compile(&mir)?;
                let result_raw = aot_module.run()?;
                let value = Value::from_raw(result_raw);
                Ok((value, module_type))
            }
            _ => {
''',
    '''        match backend() {
            #[cfg(feature = "native-codegen")]
            "native" => {
                let aot_module = crate::aot::AotModule::compile(&mir)?;
                let result_raw = aot_module.run()?;
                let value = Value::from_raw(result_raw);
                Ok((value, module_type))
            }
            #[cfg(not(feature = "native-codegen"))]
            "native" => Err(NuError::VMError {
                msg: "native test backend not compiled in (enable 'native-codegen')".into(),
                span: crate::types::Span::default(),
            }),
            _ => {
''',
)

print("current-main mobile runtime reconciliation applied")
