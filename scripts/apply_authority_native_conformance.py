#!/usr/bin/env python3
from pathlib import Path


def read(path):
    return Path(path).read_text()


def write(path, text):
    Path(path).write_text(text)


def replace_once(text, old, new, label):
    n = text.count(old)
    if n != 1:
        raise SystemExit(f"{label}: expected 1 occurrence, found {n}")
    return text.replace(old, new, 1)


# ---------------------------------------------------------------------------
# AOT runtime: carry exact MIR-site authority through native spawn.
# ---------------------------------------------------------------------------
p = "src/aot/mod.rs"
s = read(p)

# Keep the legacy standalone helper while adding an authority-aware primitive.
s = replace_once(
    s,
    "    pub fn spawn_actor(\n"
    "        &self,\n"
    "        behavior_idx: usize,\n"
    "        init: Vec<(u64, crate::vm::Value)>,\n"
    "    ) -> Option<u64> {\n"
    "        let full = self.behavior_names.get(behavior_idx)?;",
    "    pub fn spawn_actor(\n"
    "        &self,\n"
    "        behavior_idx: usize,\n"
    "        init: Vec<(u64, crate::vm::Value)>,\n"
    "    ) -> Option<u64> {\n"
    "        let authority = crate::authority::AuthorityManifest::new();\n"
    "        self.spawn_actor_with_authority(behavior_idx, init, &authority)\n"
    "    }\n\n"
    "    /// Standalone native spawn with one already-validated exact-site manifest.\n"
    "    pub fn spawn_actor_with_authority(\n"
    "        &self,\n"
    "        behavior_idx: usize,\n"
    "        init: Vec<(u64, crate::vm::Value)>,\n"
    "        authority: &crate::authority::AuthorityManifest,\n"
    "    ) -> Option<u64> {\n"
    "        let full = self.behavior_names.get(behavior_idx)?;",
    "AOT authority-aware spawn method",
)
s = replace_once(
    s,
    "        let mut actor = Box::new(crate::runtime::Actor::new(id, actor_name.clone(), 64));\n\n"
    "        let prefix = format!(\"{}.\", actor_name);",
    "        let mut actor = Box::new(crate::runtime::Actor::new(id, actor_name.clone(), 64));\n"
    "        actor.install_authority_manifest(authority);\n\n"
    "        let prefix = format!(\"{}.\", actor_name);",
    "install standalone native authority",
)

# Exact-site native authority queue. Each compiled Spawn pushes only its own
# canonical tokens immediately before invoking nulang_aot_spawn.
anchor = "/// Native-code entry point for `RValue::Spawn`: creates an actor of the type\n"
if anchor not in s:
    raise SystemExit("AOT spawn anchor missing")
queue = r'''thread_local! {
    /// Canonical authority tokens for the next native Spawn instruction.
    /// The queue is site-local in generated code: tokens are pushed only after
    /// all init expressions have evaluated, then drained atomically by spawn.
    static AOT_SPAWN_AUTHORITY: std::cell::RefCell<Vec<String>> =
        const { std::cell::RefCell::new(Vec::new()) };
    /// A token that could not be resolved from the armed constant pool marks
    /// the whole pending manifest invalid; partial manifests must never grant.
    static AOT_SPAWN_AUTHORITY_INVALID: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[no_mangle]
pub unsafe extern "C" fn nulang_aot_spawn_grant_push(token_raw: u64) {
    match crate::jit::runtime::resolve_string_coerce(token_raw) {
        Some(token) => AOT_SPAWN_AUTHORITY.with(|tokens| tokens.borrow_mut().push(token)),
        None => AOT_SPAWN_AUTHORITY_INVALID.with(|invalid| invalid.set(true)),
    }
}

fn take_aot_spawn_authority() -> Option<Vec<String>> {
    let invalid = AOT_SPAWN_AUTHORITY_INVALID.with(|flag| flag.replace(false));
    let tokens = AOT_SPAWN_AUTHORITY.with(|tokens| std::mem::take(&mut *tokens.borrow_mut()));
    (!invalid).then_some(tokens)
}

'''
s = s.replace(anchor, queue + anchor, 1)

# Rewrite native spawn to validate the whole pending manifest and use the same
# attenuation primitive as the VM path.
s = replace_once(
    s,
    "pub unsafe extern \"C\" fn nulang_aot_spawn(behavior_idx: u64) -> u64 {\n"
    "    let init = crate::jit::runtime::take_aot_spawn_init();\n"
    "    let dispatch = AOT_DISPATCH.with(|c| *c.borrow());",
    "pub unsafe extern \"C\" fn nulang_aot_spawn(behavior_idx: u64) -> u64 {\n"
    "    let init = crate::jit::runtime::take_aot_spawn_init();\n"
    "    let Some(authority_tokens) = take_aot_spawn_authority() else {\n"
    "        return crate::vm::Value::nil().as_raw();\n"
    "    };\n"
    "    let requested = match crate::authority::AuthorityManifest::from_tokens(\n"
    "        authority_tokens.iter().map(String::as_str),\n"
    "    ) {\n"
    "        Ok(manifest) => manifest,\n"
    "        Err(_) => return crate::vm::Value::nil().as_raw(),\n"
    "    };\n"
    "    let dispatch = AOT_DISPATCH.with(|c| *c.borrow());",
    "native spawn manifest preflight",
)
s = replace_once(
    s,
    "                let val =\n"
    "                    unsafe { (*t.runtime).spawn_from_module(code, behavior_idx as usize, init) };\n"
    "                return val.as_raw();",
    "                let val = unsafe {\n"
    "                    crate::runtime::spawn::spawn_from_module_with_authority(\n"
    "                        &mut *t.runtime,\n"
    "                        code,\n"
    "                        behavior_idx as usize,\n"
    "                        init,\n"
    "                        &requested,\n"
    "                    )\n"
    "                };\n"
    "                return val.unwrap_or_else(|_| crate::vm::Value::nil()).as_raw();",
    "native runtime attenuation",
)
s = replace_once(
    s,
    "        return match module.spawn_actor(behavior_idx as usize, init) {",
    "        return match module.spawn_actor_with_authority(behavior_idx as usize, init, &requested) {",
    "native dispatched standalone authority",
)
s = replace_once(
    s,
    "    match (*module).spawn_actor(behavior_idx as usize, init) {",
    "    match (*module).spawn_actor_with_authority(behavior_idx as usize, init, &requested) {",
    "native fallback standalone authority",
)

# Intern canonical capability tokens in the AOT constant pool. Invalid MIR is
# still rejected during codegen; the collector simply makes valid tokens
# addressable as TAG_STRING values.
s = replace_once(
    s,
    "        mir::RValue::Spawn { init, .. } => {\n"
    "            for (name, rv) in init {",
    "        mir::RValue::Spawn {\n"
    "            init, capabilities, ..\n"
    "        } => {\n"
    "            if let Ok(manifest) = crate::authority::AuthorityManifest::from_tokens(\n"
    "                capabilities.iter().map(String::as_str),\n"
    "            ) {\n"
    "                for token in manifest.canonical_tokens() {\n"
    "                    let c = crate::bytecode::Constant::String(token);\n"
    "                    if !constants.contains(&c) {\n"
    "                        constants.push(c);\n"
    "                    }\n"
    "                }\n"
    "            }\n"
    "            for (name, rv) in init {",
    "AOT collect authority constants",
)
write(p, s)

# ---------------------------------------------------------------------------
# AOT codegen: validate typed grants and push this exact MIR site's canonical
# manifest after init evaluation, immediately before native spawn.
# ---------------------------------------------------------------------------
p = "src/aot/codegen.rs"
s = read(p)

# Declare the one-argument void helper.
s = replace_once(
    s,
    "        // spawn: (i64) -> i64 (create a standalone actor)\n"
    "        {\n"
    "            let mut h_sig = module.make_signature();\n"
    "            h_sig.params.push(AbiParam::new(types::I64));\n"
    "            h_sig.returns.push(AbiParam::new(types::I64));\n"
    "            let h_id = module\n"
    "                .declare_function(\"nulang_aot_spawn\", Linkage::Import, &h_sig)",
    "        // spawn_grant_push: (i64) -> () (queue one canonical authority token)\n"
    "        {\n"
    "            let mut h_sig = module.make_signature();\n"
    "            h_sig.params.push(AbiParam::new(types::I64));\n"
    "            let h_id = module\n"
    "                .declare_function(\"nulang_aot_spawn_grant_push\", Linkage::Import, &h_sig)\n"
    "                .map_err(|e| AotCompileError::Cranelift(e.to_string()))?;\n"
    "            let func_ref = module.declare_func_in_func(h_id, builder.func);\n"
    "            h.insert(\"nulang_aot_spawn_grant_push\", func_ref);\n"
    "        }\n"
    "        // spawn: (i64) -> i64 (create a standalone actor)\n"
    "        {\n"
    "            let mut h_sig = module.make_signature();\n"
    "            h_sig.params.push(AbiParam::new(types::I64));\n"
    "            h_sig.returns.push(AbiParam::new(types::I64));\n"
    "            let h_id = module\n"
    "                .declare_function(\"nulang_aot_spawn\", Linkage::Import, &h_sig)",
    "declare AOT grant helper",
)

s = replace_once(
    s,
    "        mir::RValue::Spawn {\n"
    "            behavior_idx,\n"
    "            init,\n"
    "            target_node,\n"
    "            capabilities: _,\n"
    "        } => {",
    "        mir::RValue::Spawn {\n"
    "            behavior_idx,\n"
    "            init,\n"
    "            target_node,\n"
    "            capabilities,\n"
    "        } => {",
    "AOT retain spawn capabilities",
)
# Push after init expressions so a nested init Spawn cannot consume its
# parent's queued authority.
s = replace_once(
    s,
    "            let behavior_val = builder.ins().iconst(types::I64, *behavior_idx as i64);\n"
    "            call_helper(builder, helpers, \"nulang_aot_spawn\", &[behavior_val])",
    "            let manifest = crate::authority::AuthorityManifest::from_tokens(\n"
    "                capabilities.iter().map(String::as_str),\n"
    "            )\n"
    "            .map_err(|err| {\n"
    "                AotCompileError::Internal(format!(\n"
    "                    \"invalid spawn authority grant in native backend: {err}\"\n"
    "                ))\n"
    "            })?;\n"
    "            for token in manifest.canonical_tokens() {\n"
    "                let token_val = compile_const(\n"
    "                    builder,\n"
    "                    &crate::bytecode::Constant::String(token),\n"
    "                    mode,\n"
    "                    constants,\n"
    "                )?;\n"
    "                call_void_helper(\n"
    "                    builder,\n"
    "                    helpers,\n"
    "                    \"nulang_aot_spawn_grant_push\",\n"
    "                    &[token_val],\n"
    "                )?;\n"
    "            }\n"
    "            let behavior_val = builder.ins().iconst(types::I64, *behavior_idx as i64);\n"
    "            call_helper(builder, helpers, \"nulang_aot_spawn\", &[behavior_val])",
    "AOT exact-site grant push",
)
write(p, s)

# ---------------------------------------------------------------------------
# Delete behavior-index compatibility bridge now that VM uses exact bytecode
# PC and native code carries its exact MIR site's manifest directly.
# ---------------------------------------------------------------------------
p = "src/authority_runtime.rs"
s = read(p)
s = s.replace("use crate::bytecode::{CodeModule, OpCode};", "use crate::bytecode::CodeModule;")
old_variant = r'''    /// More than one local `Spawn` instruction targets the same behavior and
    /// at least one of those sites carries authority metadata.
    ///
    /// The current VM callback receives `(module, behavior_idx, init)` but not
    /// the executing spawn PC. Until that PC is threaded through the callback,
    /// per-site authority can only be inferred safely when a privileged target
    /// behavior has exactly one local spawn instruction in the module.
    AmbiguousSpawnSite { behavior_idx: usize },
'''
if old_variant not in s:
    raise SystemExit("compat error variant block missing")
s = s.replace(old_variant, "", 1)
s = replace_once(
    s,
    "            RuntimeAuthorityError::AmbiguousSpawnSite { behavior_idx } => {\n"
    "                write!(\n"
    "                    f,\n"
    "                    \"cannot infer spawn authority: behavior {behavior_idx} has multiple local spawn sites\"\n"
    "                )\n"
    "            }\n",
    "",
    "remove compatibility Display branch",
)
s = replace_once(
    s,
    "            RuntimeAuthorityError::AmbiguousSpawnMetadata { .. }\n"
    "            | RuntimeAuthorityError::AmbiguousSpawnSite { .. }\n"
    "            | RuntimeAuthorityError::Denied(_) => None,",
    "            RuntimeAuthorityError::AmbiguousSpawnMetadata { .. }\n"
    "            | RuntimeAuthorityError::Denied(_) => None,",
    "remove compatibility Error source branch",
)
start = s.index("/// Resolve spawn authority using only the context exposed by the current VM")
end = s.index("impl Actor {", start)
s = s[:start] + s[end:]
# Remove bridge-only test helper/import/tests as one contiguous block between
# duplicate-metadata test and actor_authority test.
s = s.replace("    use crate::bytecode::Instruction;\n", "")
helper_start = s.find("    fn emit_spawn(module: &mut CodeModule")
if helper_start >= 0:
    helper_end = s.index("    #[test]", helper_start)
    s = s[:helper_start] + s[helper_end:]
bridge_start = s.find("    #[test]\n    fn callback_compatibility_resolves_unique_spawn_site()")
actor_test = s.find("    #[test]\n    fn actor_authority_is_deny_by_default()")
if bridge_start < 0 or actor_test < 0 or actor_test <= bridge_start:
    raise SystemExit("compat tests block boundaries missing")
s = s[:bridge_start] + s[actor_test:]
write(p, s)

# ---------------------------------------------------------------------------
# Differential conformance tests: same authority-bearing program through VM
# bytecode and native Cranelift; same denied-parent behavior through both.
# ---------------------------------------------------------------------------
p = "tests/spawn_authority_provenance.rs"
s = read(p)
s = replace_once(
    s,
    "use nulang::bytecode::OpCode;\n",
    "use nulang::aot::AotModule;\nuse nulang::bytecode::OpCode;\n",
    "test AOT import",
)
append = r'''

fn sorted_actor_manifests(rt: &Runtime) -> Vec<Vec<String>> {
    let mut manifests: Vec<Vec<String>> = rt
        .actors
        .values()
        .map(|actor| actor.authority_manifest().unwrap().canonical_tokens())
        .collect();
    manifests.sort();
    manifests
}

#[test]
fn vm_and_native_agree_on_exact_site_spawn_authority() {
    let source = r#"
actor Child { behavior ping() { 1 } }
fn main() {
    let first = spawn Child {} with [Secret::Read("FIRST_KEY")]
    let second = spawn Child {} with [Secret::Read("SECOND_KEY")]
    second
}
"#;

    let mut vm_mir = lower(source);
    let vm_module = compile_mir(&mut vm_mir, "authority-vm-native-vm").unwrap();
    let vm_runtime = Rc::new(RefCell::new(Runtime::new()));
    let mut vm = VM::new();
    vm.load_module(vm_module);
    vm.set_actor_callbacks(Box::new(RuntimeVmCallbacks::new(vm_runtime.clone())));
    vm.run().unwrap();
    let vm_manifests = sorted_actor_manifests(&vm_runtime.borrow());

    let native_mir = lower(source);
    let aot = AotModule::compile(&native_mir).unwrap();
    let mut native_runtime = Runtime::new();
    aot.run_in_runtime(&mut native_runtime).unwrap();
    let native_manifests = sorted_actor_manifests(&native_runtime);

    let expected = vec![
        vec!["Secret::Read(FIRST_KEY)".to_string()],
        vec!["Secret::Read(SECOND_KEY)".to_string()],
    ];
    assert_eq!(vm_manifests, expected);
    assert_eq!(native_manifests, expected);
}

#[test]
fn vm_and_native_both_reject_parent_authority_escalation_before_creation() {
    let source = r#"
actor Child { behavior ping() { 1 } }
fn main() { spawn Child {} with [Secret::Read("UNHELD_KEY")] }
"#;
    const PARENT: u64 = 900_101;

    let mut vm_mir = lower(source);
    let vm_module = compile_mir(&mut vm_mir, "authority-vm-native-denied-vm").unwrap();
    let vm_runtime = Rc::new(RefCell::new(Runtime::new()));
    {
        let mut rt = vm_runtime.borrow_mut();
        rt.actors
            .insert(PARENT, Actor::new(PARENT, "unprivileged-parent", 0));
        rt.current_actor = Some(PARENT);
    }
    let mut vm = VM::new();
    vm.load_module(vm_module);
    vm.set_actor_callbacks(Box::new(RuntimeVmCallbacks::new(vm_runtime.clone())));
    assert!(vm.run().unwrap().is_nil());
    assert_eq!(vm_runtime.borrow().actors.len(), 1);

    let native_mir = lower(source);
    let aot = AotModule::compile(&native_mir).unwrap();
    let mut native_runtime = Runtime::new();
    native_runtime
        .actors
        .insert(PARENT, Actor::new(PARENT, "unprivileged-parent", 0));
    native_runtime.current_actor = Some(PARENT);
    let raw = aot.run_in_runtime(&mut native_runtime).unwrap();
    assert!(nulang::vm::Value::from_bits(raw).is_nil());
    assert_eq!(native_runtime.actors.len(), 1);
    assert!(native_runtime.actors.contains_key(&PARENT));
}
'''
s += append
write(p, s)

# Assertions preventing a partial native fix.
assert "capabilities: _," not in read("src/aot/codegen.rs")
assert "nulang_aot_spawn_grant_push" in read("src/aot/codegen.rs")
assert "spawn_from_module_with_authority" in read("src/aot/mod.rs")
assert "spawn_authority_manifest_for_behavior" not in read("src/authority_runtime.rs")
assert "AmbiguousSpawnSite" not in read("src/authority_runtime.rs")
assert "vm_and_native_agree_on_exact_site_spawn_authority" in read("tests/spawn_authority_provenance.rs")
print("native authority conformance patch applied")
