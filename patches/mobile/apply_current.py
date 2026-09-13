#!/usr/bin/env python3
from pathlib import Path
import sys


def replace_once(path: str, old: str, new: str) -> None:
    p = Path(path)
    text = p.read_text()
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{path}: expected exactly one match, found {count}: {old[:100]!r}")
    p.write_text(text.replace(old, new, 1))


def write_new(path: str, content: str) -> None:
    p = Path(path)
    if p.exists():
        raise SystemExit(f"{path}: expected new file but it already exists")
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text(content)


# Cargo feature boundary: the embedded mobile profile keeps only the interpreter
# and base runtime. Default behavior remains unchanged by enabling native-codegen
# in the default feature set.
replace_once(
    "Cargo.toml",
    'default = ["python", "sqlite", "lsp", "ai-runtime", "tls", "ffi", "tcp", "ureq"]\n',
    'default = ["native-codegen", "python", "sqlite", "lsp", "ai-runtime", "tls", "ffi", "tcp", "ureq"]\n'
    '# Cranelift-backed tiered JIT plus the current native/AOT backend.\n'
    'native-codegen = [\n'
    '    "dep:cranelift",\n'
    '    "dep:cranelift-jit",\n'
    '    "dep:cranelift-module",\n'
    '    "dep:cranelift-native",\n'
    '    "dep:cranelift-frontend",\n'
    '    "dep:cranelift-codegen",\n'
    '    "dep:target-lexicon",\n'
    ']\n'
    '# Marker profile for embedded mobile runtimes. Intentionally empty: the\n'
    '# bytecode interpreter and pre-registered host callbacks need no JIT.\n'
    'mobile-runtime = []\n'
)
replace_once(
    "Cargo.toml",
    'wasm-backend = ["dep:wasm-encoder", "dep:wasmtime", "dep:wat", "dep:borsh"]',
    'wasm-backend = ["native-codegen", "dep:wasm-encoder", "dep:wasmtime", "dep:wat", "dep:borsh"]',
)
replace_once("Cargo.toml", 'difffuzz = []', 'difffuzz = ["native-codegen"]')
for name in [
    "cranelift",
    "cranelift-jit",
    "cranelift-module",
    "cranelift-native",
    "cranelift-frontend",
]:
    replace_once("Cargo.toml", f'{name} = "0.132"', f'{name} = {{ version = "0.132", optional = true }}')
replace_once(
    "Cargo.toml",
    'cranelift-codegen = { version = "0.132", features = ["riscv64"] }',
    'cranelift-codegen = { version = "0.132", features = ["riscv64"], optional = true }',
)
replace_once(
    "Cargo.toml",
    'target-lexicon = "0.13"',
    'target-lexicon = { version = "0.13", optional = true }',
)

# Compile native-codegen implementation modules only when requested.
for module in ["aot", "cranelift_utils", "difffuzz", "fuzz", "jit"]:
    replace_once(
        "src/lib.rs",
        f"pub mod {module};",
        f'#[cfg(feature = "native-codegen")]\npub mod {module};',
    )

# Keep the JIT trait surface available to the interpreter while making the
# concrete Cranelift implementation optional.
replace_once(
    "src/backends/mod.rs",
    '''/// Create the default JIT backend (Cranelift via `JitSession`).
///
/// This is the **sole** call-site for `JitSession::new()` outside of tests.
/// The VM calls this factory rather than importing `JitSession` directly,
/// keeping the JIT implementation behind the `JitBackend` trait boundary.
pub fn create_default_jit() -> Option<Box<dyn JitBackend>> {
    crate::jit::JitSession::new().map(|j| Box::new(j) as Box<dyn JitBackend>)
}
''',
    '''/// Scheduler budget used by native-codegen safepoints.
///
/// This lives outside `crate::jit` so interpreter-only runtimes can retain the
/// actor bookkeeping fields without importing the Cranelift module tree.
pub const JIT_SAFEPOINT_BUDGET: u64 = 1000;

/// Create the default JIT backend when native code generation is compiled in.
#[cfg(feature = "native-codegen")]
pub fn create_default_jit() -> Option<Box<dyn JitBackend>> {
    crate::jit::JitSession::new().map(|j| Box::new(j) as Box<dyn JitBackend>)
}

/// Interpreter-only builds preserve the same VM trait boundary but have no
/// native tier to instantiate.
#[cfg(not(feature = "native-codegen"))]
pub fn create_default_jit() -> Option<Box<dyn JitBackend>> {
    None
}
''',
)

# Actor layout keeps generic scheduling state but removes concrete AOT types.
replace_once(
    "src/runtime/actor.rs",
    '    pub aot_targets: Vec<Option<crate::aot::AotDispatchTarget>>,',
    '    #[cfg(feature = "native-codegen")]\n    pub aot_targets: Vec<Option<crate::aot::AotDispatchTarget>>,',
)
replace_once(
    "src/runtime/actor.rs",
    '            aot_targets: Vec::new(),',
    '            #[cfg(feature = "native-codegen")]\n            aot_targets: Vec::new(),',
)
replace_once(
    "src/runtime/actor.rs",
    'crate::jit::runtime::JIT_SAFEPOINT_BUDGET',
    'crate::backends::JIT_SAFEPOINT_BUDGET',
)

# Runtime owns concrete AOT state only in native-codegen builds.
replace_once(
    "src/runtime/mod.rs",
    '    pub aot_modules: std::collections::HashMap<String, *const crate::aot::AotModule>,',
    '    #[cfg(feature = "native-codegen")]\n    pub aot_modules: std::collections::HashMap<String, *const crate::aot::AotModule>,',
)
replace_once(
    "src/runtime/mod.rs",
    '    pub aot_module_storage: Vec<Box<crate::aot::AotModule>>,',
    '    #[cfg(feature = "native-codegen")]\n    pub aot_module_storage: Vec<Box<crate::aot::AotModule>>,',
)
replace_once(
    "src/runtime/mod.rs",
    '            aot_modules: std::collections::HashMap::new(),\n            aot_module_storage: Vec::new(),',
    '            #[cfg(feature = "native-codegen")]\n            aot_modules: std::collections::HashMap::new(),\n            #[cfg(feature = "native-codegen")]\n            aot_module_storage: Vec::new(),',
)
replace_once(
    "src/runtime/mod.rs",
    '    fn resume_suspended_jit_yield(&mut self, actor_id: u64) {',
    '    #[cfg(feature = "native-codegen")]\n    fn resume_suspended_jit_yield(&mut self, actor_id: u64) {',
)
# All references inside the cfg-gated function use the neutral constant.
p = Path("src/runtime/mod.rs")
text = p.read_text().replace(
    'crate::jit::runtime::JIT_SAFEPOINT_BUDGET',
    'crate::backends::JIT_SAFEPOINT_BUDGET',
)
p.write_text(text)
replace_once(
    "src/runtime/mod.rs",
    '''        let jit_yield = self
            .actors
            .get(&actor_id)
            .map(|a| a.jit_yield_pending)
            .unwrap_or(false);
        if jit_yield {
            self.resume_suspended_jit_yield(actor_id);
        }
''',
    '''        #[cfg(feature = "native-codegen")]
        {
            let jit_yield = self
                .actors
                .get(&actor_id)
                .map(|a| a.jit_yield_pending)
                .unwrap_or(false);
            if jit_yield {
                self.resume_suspended_jit_yield(actor_id);
            }
        }
''',
)
replace_once(
    "src/runtime/mod.rs",
    '''            let aot_target = self
                .actors
                .get(&actor_id)
                .and_then(|a| a.aot_targets.get(behavior_idx))
                .and_then(|t| *t);
''',
    '''            #[cfg(feature = "native-codegen")]
            let aot_target = self
                .actors
                .get(&actor_id)
                .and_then(|a| a.aot_targets.get(behavior_idx))
                .and_then(|t| *t);
''',
)
replace_once(
    "src/runtime/mod.rs",
    '''                    if let Some(target) = aot_target {
                        crate::aot::set_aot_dispatch(Some(target));
                    }
                    handler(actor, &msg.payload);
                    if aot_target.is_some() {
                        crate::aot::clear_aot_dispatch();
                    }
''',
    '''                    #[cfg(feature = "native-codegen")]
                    if let Some(target) = aot_target {
                        crate::aot::set_aot_dispatch(Some(target));
                    }
                    handler(actor, &msg.payload);
                    #[cfg(feature = "native-codegen")]
                    if aot_target.is_some() {
                        crate::aot::clear_aot_dispatch();
                    }
''',
)
replace_once(
    "src/runtime/mod.rs",
    '''            // Reset JIT safepoint counter for this behavior invocation.
            if let Some(actor) = self.actors.get_mut(&actor_id) {
                actor.jit_safepoint_counter = crate::backends::JIT_SAFEPOINT_BUDGET;
                crate::jit::runtime::set_jit_safepoint_ptr(&mut actor.jit_safepoint_counter);
            }
''',
    '''            #[cfg(feature = "native-codegen")]
            {
                // Reset native-codegen safepoint counter for this behavior.
                if let Some(actor) = self.actors.get_mut(&actor_id) {
                    actor.jit_safepoint_counter = crate::backends::JIT_SAFEPOINT_BUDGET;
                    crate::jit::runtime::set_jit_safepoint_ptr(
                        &mut actor.jit_safepoint_counter,
                    );
                }
            }
''',
)
replace_once(
    "src/runtime/mod.rs",
    '''            // JIT safepoint yield: capture state for inline resume on next turn.
            if vm.yield_pending {
                if let Some(vm_state) = vm.take_suspended_state() {
                    if let Some(actor) = self.actors.get_mut(&actor_id) {
                        actor.suspended_execution =
                            Some(crate::runtime::actor::SuspendedExecution {
                                vm_state,
                                behavior_idx: 0,
                                step_name: String::new(),
                            });
                        actor.jit_yield_pending = true;
                    }
                }
                crate::jit::runtime::clear_jit_safepoint_ptr();
                (*self_ptr).vm_exec_end();
                return Ok(Value::nil());
            }
''',
    '''            #[cfg(feature = "native-codegen")]
            {
                // JIT safepoint yield: capture state for inline resume.
                if vm.yield_pending {
                    if let Some(vm_state) = vm.take_suspended_state() {
                        if let Some(actor) = self.actors.get_mut(&actor_id) {
                            actor.suspended_execution =
                                Some(crate::runtime::actor::SuspendedExecution {
                                    vm_state,
                                    behavior_idx: 0,
                                    step_name: String::new(),
                                });
                            actor.jit_yield_pending = true;
                        }
                    }
                    crate::jit::runtime::clear_jit_safepoint_ptr();
                    (*self_ptr).vm_exec_end();
                    return Ok(Value::nil());
                }
            }
''',
)
replace_once(
    "src/runtime/mod.rs",
    '            crate::jit::runtime::clear_jit_safepoint_ptr();\n            // String-id values index into this runtime VM',
    '            #[cfg(feature = "native-codegen")]\n            crate::jit::runtime::clear_jit_safepoint_ptr();\n            // String-id values index into this runtime VM',
)
replace_once(
    "src/runtime/mod.rs",
    '    pub fn register_aot_module(\n',
    '    #[cfg(feature = "native-codegen")]\n    pub fn register_aot_module(\n',
)

# AOT behavior wiring is absent from interpreter-only builds.
replace_once(
    "src/runtime/spawn.rs",
    '''    // Wire AOT-native dispatch: if an AOT module is registered for this actor
    // type, register the adapter for each behavior it compiles so the
    // scheduler dispatches them natively (bytecode falls back for the rest).
    if let Some(meta) = meta.as_ref() {
''',
    '''    // Wire AOT-native dispatch only when native codegen is present.
    #[cfg(feature = "native-codegen")]
    if let Some(meta) = meta.as_ref() {
''',
)

# The VM retains a stable JitBackend trait slot; concrete execution helpers are
# compiled only when native-codegen exists.
replace_once(
    "src/vm.rs",
    '    fn try_jit_execute(&mut self, frame_idx: usize) -> bool {',
    '    #[cfg(feature = "native-codegen")]\n    fn try_jit_execute(&mut self, frame_idx: usize) -> bool {',
)
replace_once(
    "src/vm.rs",
    '''        // JIT fell back to interpretation — continue in the interpreter.
        false
    }

    /// Execute a single bytecode instruction.
''',
    '''        // JIT fell back to interpretation — continue in the interpreter.
        false
    }

    #[cfg(not(feature = "native-codegen"))]
    fn try_jit_execute(&mut self, _frame_idx: usize) -> bool {
        false
    }

    /// Execute a single bytecode instruction.
''',
)
replace_once(
    "src/vm.rs",
    '    pub(crate) fn jit_direct_call(\n',
    '    #[cfg(feature = "native-codegen")]\n    pub(crate) fn jit_direct_call(\n',
)

# CLI and benches must still compile under --no-default-features.
replace_once(
    "src/main.rs",
    '        "native" => {\n',
    '        #[cfg(feature = "native-codegen")]\n        "native" => {\n',
)
replace_once(
    "src/main.rs",
    '''            Ok(())
        }
        "bytecode" => {
''',
    '''            Ok(())
        }
        #[cfg(not(feature = "native-codegen"))]
        "native" => Err(nulang::types::NuError::VMError {
            msg: "native backend not compiled in (enable 'native-codegen' feature)".into(),
            span: Span::default(),
        }),
        "bytecode" => {
''',
)
replace_once(
    "src/main.rs",
    '    print!("  --backend <b>    Backend: bytecode (default) | native | core-vm");\n',
    '    print!("  --backend <b>    Backend: bytecode (default) | core-vm");\n    if cfg!(feature = "native-codegen") {\n        print!(" | native");\n    }\n',
)
replace_once(
    "src/main.rs",
    '''    println!("                   native: pure-functional subset only (no effects,");
    println!("                   actors, or FFI — errors name the unsupported");
    println!("                   construct; use bytecode for full-language programs)");
''',
    '''    if cfg!(feature = "native-codegen") {
        println!("                   native: pure-functional subset only (no effects,");
        println!("                   actors, or FFI — errors name the unsupported");
        println!("                   construct; use bytecode for full-language programs)");
    }
''',
)
replace_once(
    "src/main.rs",
    '    println!("  --target <t>     Target ISA for native backend: native (default) | ptx | riscv64");\n',
    '    if cfg!(feature = "native-codegen") {\n        println!("  --target <t>     Target ISA for native backend: native (default) | ptx | riscv64");\n    }\n',
)
replace_once(
    "benches/bench_main.rs",
    'mod aot_bench;',
    '#[cfg(feature = "native-codegen")]\nmod aot_bench;',
)
replace_once(
    "benches/bench_main.rs",
    'mod jit_bench;',
    '#[cfg(feature = "native-codegen")]\nmod jit_bench;',
)
replace_once(
    "benches/bench_main.rs",
    '''criterion_main!(
    vm_bench::benches,
    interp_bench::benches,
    actor_bench::benches,
    aot_bench::benches,
    jit_bench::benches,
    gc_bench::benches,
    dist_bench::benches,
    persist_bench::benches,
);
''',
    '''#[cfg(feature = "native-codegen")]
criterion_main!(
    vm_bench::benches,
    interp_bench::benches,
    actor_bench::benches,
    aot_bench::benches,
    jit_bench::benches,
    gc_bench::benches,
    dist_bench::benches,
    persist_bench::benches,
);

#[cfg(not(feature = "native-codegen"))]
criterion_main!(
    vm_bench::benches,
    interp_bench::benches,
    actor_bench::benches,
    gc_bench::benches,
    dist_bench::benches,
    persist_bench::benches,
);
''',
)

write_new(
    "scripts/check_mobile_runtime_profile.sh",
    '''#!/usr/bin/env bash
set -euo pipefail

FEATURES="mobile-runtime"

echo "==> Checking interpreter-only mobile runtime"
cargo check --lib --no-default-features --features "$FEATURES"

echo "==> Running library tests for mobile runtime profile"
cargo test --lib --no-default-features --features "$FEATURES"

echo "==> Auditing runtime dependency graph"
tree="$(cargo tree --no-default-features --features "$FEATURES" --edges normal,build)"
printf '%s\\n' "$tree"

forbidden='(^|[[:space:]├└│─])(cranelift([[:alnum:]_-]*)?|wasmtime|libloading)[[:space:]]v'
if printf '%s\\n' "$tree" | grep -Eiq "$forbidden"; then
  echo "ERROR: mobile-runtime dependency graph contains native codegen/dynamic-loader crates" >&2
  printf '%s\\n' "$tree" | grep -Ei "$forbidden" >&2 || true
  exit 1
fi

metadata="$(cargo metadata --format-version 1 --no-deps)"
python3 - "$metadata" <<'PY'
import json, sys
metadata = json.loads(sys.argv[1])
root = next(p for p in metadata["packages"] if p["name"] == "nulang")
features = root["features"]
required = {
    "dep:cranelift", "dep:cranelift-jit", "dep:cranelift-module",
    "dep:cranelift-native", "dep:cranelift-frontend", "dep:cranelift-codegen",
    "dep:target-lexicon",
}
missing = sorted(required - set(features.get("native-codegen", [])))
if missing:
    raise SystemExit("native-codegen missing dependency ownership: " + ", ".join(missing))
mobile = set(features.get("mobile-runtime", []))
forbidden = sorted(x for x in mobile if x == "native-codegen" or x == "ffi" or x.startswith("dep:cranelift") or x in {"dep:wasmtime", "dep:libloading"})
if forbidden:
    raise SystemExit("mobile-runtime enables forbidden features: " + ", ".join(forbidden))
if "native-codegen" not in set(features.get("default", [])):
    raise SystemExit("default builds must retain native-codegen")
print("mobile-runtime feature ownership: OK")
PY

echo "mobile-runtime profile: OK"
''',
)

# CI proves both source-level compilation and dependency-graph isolation.
replace_once(
    ".github/workflows/ci.yml",
    '''  # -----------------------------------------------------------------------
  # WASM backend
  # -----------------------------------------------------------------------
  wasm-backend:
''',
    '''  # -----------------------------------------------------------------------
  # Mobile runtime — interpreter-only and dependency-graph constrained
  # -----------------------------------------------------------------------
  mobile-runtime:
    name: Mobile Runtime (no native codegen)
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v7

      - name: Install Rust
        uses: dtolnay/rust-toolchain@stable

      - name: Cache cargo
        uses: Swatinem/rust-cache@v2

      - name: Verify mobile runtime profile
        run: bash scripts/check_mobile_runtime_profile.sh

  # -----------------------------------------------------------------------
  # WASM backend
  # -----------------------------------------------------------------------
  wasm-backend:
''',
)

write_new(
    "docs/web/MOBILE_RUNTIME_RELEASE_GATE.md",
    '''# Mobile runtime release gate

The `mobile-runtime` Cargo profile is a distribution boundary, not merely a
runtime setting.

Every change must preserve all of the following:

1. `cargo check --lib --no-default-features --features mobile-runtime` passes.
2. Library tests pass under the same feature set.
3. The normal/build dependency tree contains no Cranelift crates, Wasmtime, or
   `libloading`.
4. Cranelift and `target-lexicon` root dependencies remain owned by the
   `native-codegen` feature.
5. `mobile-runtime` does not enable `native-codegen` or dynamic FFI loading.
6. Default builds retain `native-codegen` until that public default changes
   intentionally.

The canonical local/CI command is:

```sh
bash scripts/check_mobile_runtime_profile.sh
```

## Why this is required

Disabling JIT execution at runtime is insufficient for an Apple binary: code
for runtime executable-code generation can still be linked into the artifact.
The mobile profile therefore removes native-codegen dependencies from the Cargo
graph and also compiles/tests the resulting interpreter-only source graph.

Do not publish an Apple static library or XCFramework unless this gate is green
on the exact commit being packaged.
''',
)

print("current-main mobile runtime boundary applied")
