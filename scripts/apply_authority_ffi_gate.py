#!/usr/bin/env python3
from pathlib import Path


def replace_once(text: str, old: str, new: str, label: str) -> str:
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected 1 occurrence, found {count}")
    return text.replace(old, new, 1)

# ---------------------------------------------------------------------------
# VM callback contract + interpreter FFICall enforcement.
# ---------------------------------------------------------------------------
vm_path = Path("src/vm.rs")
vm = vm_path.read_text()
vm = replace_once(
    vm,
    '''    /// Emit an event in the current actor.  Default is a no-op.\n    fn emit_event(&mut self, _event: &str, _args: &[Value]) {}\n\n    /// Handle a built-in effect performed without an explicit handler.''',
    '''    /// Emit an event in the current actor.  Default is a no-op.\n    fn emit_event(&mut self, _event: &str, _args: &[Value]) {}\n\n    /// Authorize one foreign-function call before any library is loaded or\n    /// symbol resolved. Standalone callbacks retain the historic ambient\n    /// behavior; runtime-backed actor callbacks override this and require an\n    /// exact typed `FFI::Call(\"library::symbol\")` authority grant.\n    fn authorize_ffi(&mut self, _library: &str, _symbol: &str) -> bool {\n        true\n    }\n\n    /// Handle a built-in effect performed without an explicit handler.''',
    "ActorVmCallbacks authorize_ffi contract",
)
vm = replace_once(
    vm,
    '''        // FFI sandbox: deny calls to libraries not in the allow-list.\n        if self.ffi_sandbox && !self.ffi_allowlist.contains(&def.library) {\n            return Err(NuError::VMError {\n                msg: format!(\n                    "FFI sandbox blocked call to '{}' from library '{}': library not in allow-list",\n                    def.symbol, def.library\n                ),\n                span: Span::default(),\n            });\n        }\n\n        let params: Vec<CType> = def''',
    '''        // FFI sandbox: deny calls to libraries not in the allow-list.\n        if self.ffi_sandbox && !self.ffi_allowlist.contains(&def.library) {\n            return Err(NuError::VMError {\n                msg: format!(\n                    "FFI sandbox blocked call to '{}' from library '{}': library not in allow-list",\n                    def.symbol, def.library\n                ),\n                span: Span::default(),\n            });\n        }\n\n        // Actor authority is checked before parameter marshalling, dynamic\n        // library loading, symbol resolution, or the native call itself.\n        if !self\n            .actor_callbacks\n            .authorize_ffi(&def.library, &def.symbol)\n        {\n            self.frames[frame_idx].regs[dst as usize] = Value::nil();\n            return Ok(());\n        }\n\n        let params: Vec<CType> = def''',
    "interpreter FFICall authority gate",
)
vm_path.write_text(vm)

# ---------------------------------------------------------------------------
# Runtime-backed callback implementations share one typed FFI authority gate.
# ---------------------------------------------------------------------------
cb_path = Path("src/runtime/callbacks.rs")
cb = cb_path.read_text()
anchor = '''/// Shared Web effect host implementation for all runtime callback types.\n/// Mirrors the standalone VM dispatch in `src/vm.rs`.\npub(crate) fn perform_web_builtin('''
ffi_helper = '''/// Enforce an exact actor-backed foreign-function authority grant.\n///\n/// The grant argument includes both library and symbol so permission to call\n/// one imported function cannot authorize another symbol from the same\n/// dynamic library. Actor-free execution keeps the legacy ambient contract.\npub(crate) fn authorize_actor_ffi(\n    rt: &Runtime,\n    actor_id: Option<u64>,\n    library: &str,\n    symbol: &str,\n) -> Result<(), String> {\n    let Some(actor_id) = actor_id else {\n        return Ok(());\n    };\n    if library.is_empty() || symbol.is_empty() {\n        return Err("FFI library and symbol must be non-empty".to_string());\n    }\n    let actor = rt\n        .actors\n        .get(&actor_id)\n        .ok_or_else(|| format!("authority source actor {actor_id} is missing"))?;\n    let grant = crate::authority::AuthorityGrant::Other {\n        namespace: "FFI".to_string(),\n        operation: "Call".to_string(),\n        argument: Some(format!("{library}::{symbol}")),\n    };\n    actor.require_authority(&grant).map_err(|error| error.to_string())\n}\n\n'''
if cb.count(anchor) != 1:
    raise SystemExit(f"FFI host helper anchor drift: {cb.count(anchor)}")
cb = cb.replace(anchor, ffi_helper + anchor, 1)

cb = replace_once(
    cb,
    '''impl crate::vm::ActorVmCallbacks for RuntimeVmCallbacks {\n    fn current_actor_id(&self) -> Option<u64> {\n        self.runtime.borrow().current_actor\n    }\n''',
    '''impl crate::vm::ActorVmCallbacks for RuntimeVmCallbacks {\n    fn current_actor_id(&self) -> Option<u64> {\n        self.runtime.borrow().current_actor\n    }\n\n    fn authorize_ffi(&mut self, library: &str, symbol: &str) -> bool {\n        let rt = self.runtime.borrow();\n        authorize_actor_ffi(&rt, rt.current_actor, library, symbol).is_ok()\n    }\n''',
    "RuntimeVmCallbacks FFI gate",
)
cb = replace_once(
    cb,
    '''impl crate::vm::ActorVmCallbacks for BytecodeRuntimeCallbacks {\n    fn current_actor_id(&self) -> Option<u64> {\n        Some(self.actor_id)\n    }\n''',
    '''impl crate::vm::ActorVmCallbacks for BytecodeRuntimeCallbacks {\n    fn current_actor_id(&self) -> Option<u64> {\n        Some(self.actor_id)\n    }\n\n    fn authorize_ffi(&mut self, library: &str, symbol: &str) -> bool {\n        unsafe { authorize_actor_ffi(&*self.runtime, Some(self.actor_id), library, symbol).is_ok() }\n    }\n''',
    "BytecodeRuntimeCallbacks FFI gate",
)

# Extend the generated host authority test module with exact FFI checks.
needle = '''    #[test]\n    fn actor_free_runtime_keeps_existing_ambient_host_contract() {'''
ffi_test = '''    #[test]\n    fn actor_ffi_authority_is_exact_library_and_symbol() {\n        let mut rt = Runtime::new();\n        let actor_id = 800_004;\n        let mut actor = Actor::new(actor_id, "host-ffi-authority", 8);\n        let manifest = AuthorityManifest::from_tokens([\n            "FFI::Call(libpayments.so::charge)"\n        ])\n        .unwrap();\n        actor.install_authority_manifest(&manifest);\n        rt.actors.insert(actor_id, actor);\n\n        assert!(super::authorize_actor_ffi(\n            &rt,\n            Some(actor_id),\n            "libpayments.so",\n            "charge",\n        )\n        .is_ok());\n        assert!(super::authorize_actor_ffi(\n            &rt,\n            Some(actor_id),\n            "libpayments.so",\n            "refund",\n        )\n        .is_err());\n        assert!(super::authorize_actor_ffi(\n            &rt,\n            Some(actor_id),\n            "libother.so",\n            "charge",\n        )\n        .is_err());\n    }\n\n'''
if cb.count(needle) != 1:
    raise SystemExit(f"FFI test anchor drift: {cb.count(needle)}")
cb = cb.replace(needle, ffi_test + needle, 1)
cb_path.write_text(cb)

# ---------------------------------------------------------------------------
# AOT FFI helper must consult the same callback gate before loading/calling.
# ---------------------------------------------------------------------------
jit_path = Path("src/jit/runtime.rs")
jit = jit_path.read_text()
jit = replace_once(
    jit,
    '''fn aot_ffi_call_impl(lib_raw: u64, sym_raw: u64, sig: u64, args: &[u64]) -> Value {\n    let library = resolve_string_coerce(lib_raw).unwrap_or_default();\n    let symbol = resolve_string_coerce(sym_raw).unwrap_or_default();\n    let ret_tag = sig & 0b111;''',
    '''fn aot_ffi_call_impl(lib_raw: u64, sym_raw: u64, sig: u64, args: &[u64]) -> Value {\n    let library = resolve_string_coerce(lib_raw).unwrap_or_default();\n    let symbol = resolve_string_coerce(sym_raw).unwrap_or_default();\n    // Match interpreter FFICall: fail closed before dynamic library loading\n    // or symbol resolution when an actor lacks the exact typed grant.\n    if !unsafe { try_with_callbacks(|cb| cb.authorize_ffi(&library, &symbol)) }.unwrap_or(true) {\n        return Value::nil();\n    }\n    let ret_tag = sig & 0b111;''',
    "AOT FFI authority gate",
)
jit_path.write_text(jit)

# ---------------------------------------------------------------------------
# Native runtime callback adapters must not fall back to the trait's ambient
# default while dispatching actor code.
# ---------------------------------------------------------------------------
aot_path = Path("src/aot/mod.rs")
aot = aot_path.read_text()
aot = replace_once(
    aot,
    '''impl crate::vm::ActorVmCallbacks for AotRuntimeCallbacks {\n    fn current_actor_id(&self) -> Option<u64> {\n        Some(self.actor_id)\n    }\n''',
    '''impl crate::vm::ActorVmCallbacks for AotRuntimeCallbacks {\n    fn current_actor_id(&self) -> Option<u64> {\n        Some(self.actor_id)\n    }\n\n    fn authorize_ffi(&mut self, library: &str, symbol: &str) -> bool {\n        unsafe {\n            crate::runtime::callbacks::authorize_actor_ffi(\n                &*self.runtime,\n                Some(self.actor_id),\n                library,\n                symbol,\n            )\n            .is_ok()\n        }\n    }\n''',
    "AotRuntimeCallbacks FFI gate",
)
aot = replace_once(
    aot,
    '''impl crate::vm::ActorVmCallbacks for AotTopLevelCallbacks {\n    fn current_actor_id(&self) -> Option<u64> {\n        self.current_actor_id()\n    }\n''',
    '''impl crate::vm::ActorVmCallbacks for AotTopLevelCallbacks {\n    fn current_actor_id(&self) -> Option<u64> {\n        self.current_actor_id()\n    }\n\n    fn authorize_ffi(&mut self, library: &str, symbol: &str) -> bool {\n        unsafe {\n            let rt = &*self.runtime;\n            crate::runtime::callbacks::authorize_actor_ffi(\n                rt,\n                rt.current_actor,\n                library,\n                symbol,\n            )\n            .is_ok()\n        }\n    }\n''',
    "AotTopLevelCallbacks FFI gate",
)
aot_path.write_text(aot)

print("FFI authority gate patch applied")
