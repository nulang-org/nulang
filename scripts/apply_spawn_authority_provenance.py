#!/usr/bin/env python3
"""One-shot source patch for exact spawn authority provenance.

This script is intentionally assertion-heavy: if concurrent changes move any
security-sensitive boundary, it aborts instead of applying a fuzzy edit.
"""
from pathlib import Path
import re


def sub_once(path: str, pattern: str, replacement: str, *, flags=re.MULTILINE | re.DOTALL) -> None:
    p = Path(path)
    text = p.read_text()
    new, count = re.subn(pattern, replacement, text, count=1, flags=flags)
    if count != 1:
        raise RuntimeError(f"{path}: expected exactly one match, got {count}: {pattern[:80]!r}")
    p.write_text(new)


def replace_count(path: str, old: str, new: str, expected: int) -> None:
    p = Path(path)
    text = p.read_text()
    count = text.count(old)
    if count != expected:
        raise RuntimeError(f"{path}: expected {expected} occurrences, got {count}: {old[:80]!r}")
    p.write_text(text.replace(old, new))


# ---------------------------------------------------------------------------
# MIR -> bytecode: validate/canonicalize grants, attach them to the exact
# local Spawn PC, and reject privileged remote spawn until the wire protocol
# carries authority explicitly.
# ---------------------------------------------------------------------------
sub_once(
    "src/mir_codegen.rs",
    r"""            mir::RValue::Spawn \{
                behavior_idx,
                init,
                target_node,
                capabilities: _,
            \} => \{
                if let Some\(node\) = target_node \{
                    let node_reg = self\.local_reg\(\*node\);""",
    """            mir::RValue::Spawn {
                behavior_idx,
                init,
                target_node,
                capabilities,
            } => {
                if let Some(node) = target_node {
                    if !capabilities.is_empty() {
                        return Err(compile_err(
                            "spawn@node authority grants are unsupported until the distributed spawn protocol carries typed authority",
                            Span::default(),
                        ));
                    }
                    let node_reg = self.local_reg(*node);""",
)

sub_once(
    "src/mir_codegen.rs",
    r"""                \} else \{
                    let pc = self\.current_offset\(\);
                    self\.emit\(Instruction::new3\(
                        OpCode::Spawn,
                        \(\(\*behavior_idx >> 8\) & 0xFF\) as u8,
                        \(\*behavior_idx & 0xFF\) as u8,
                        dst,
                    \)\);""",
    """                } else {
                    let authority_manifest = crate::authority::AuthorityManifest::from_tokens(
                        capabilities.iter().map(String::as_str),
                    )
                    .map_err(|err| {
                        compile_err(
                            format!("invalid spawn authority grant: {err}"),
                            Span::default(),
                        )
                    })?;
                    let pc = self.current_offset();
                    self.emit(Instruction::new3(
                        OpCode::Spawn,
                        ((*behavior_idx >> 8) & 0xFF) as u8,
                        (*behavior_idx & 0xFF) as u8,
                        dst,
                    ));
                    if !authority_manifest.is_empty() {
                        self.module
                            .spawn_capability_grants
                            .push((pc, authority_manifest.canonical_tokens()));
                    }""",
)

# ---------------------------------------------------------------------------
# VM callback contract: exact Spawn instruction provenance is part of the
# security boundary. The VM already computes spawn_pc for init side tables.
# ---------------------------------------------------------------------------
sub_once(
    "src/vm.rs",
    r"""    fn spawn_actor\(
        &mut self,
        module: &CodeModule,
        behavior_idx: usize,
        init: Vec<\(String, Value\)>,
    \) -> Value;""",
    """    fn spawn_actor(
        &mut self,
        module: &CodeModule,
        spawn_pc: usize,
        behavior_idx: usize,
        init: Vec<(String, Value)>,
    ) -> Value;""",
)

# Standalone callback remains authority-free, but must accept provenance.
sub_once(
    "src/vm.rs",
    r"""    fn spawn_actor\(
        &mut self,
        _module: &CodeModule,
        _behavior_idx: usize,
        _init: Vec<\(String, Value\)>,
    \) -> Value \{""",
    """    fn spawn_actor(
        &mut self,
        _module: &CodeModule,
        _spawn_pc: usize,
        _behavior_idx: usize,
        _init: Vec<(String, Value)>,
    ) -> Value {""",
)

vm_path = Path("src/vm.rs")
vm_text = vm_path.read_text()
old_call = "self.actor_callbacks.spawn_actor(module, behavior_idx, init)"
call_count = vm_text.count(old_call)
if call_count < 1:
    raise RuntimeError("src/vm.rs: no local spawn callback calls found")
vm_text = vm_text.replace(
    old_call,
    "self.actor_callbacks\n                .spawn_actor(module, spawn_pc, behavior_idx, init)",
)
vm_path.write_text(vm_text)

# ---------------------------------------------------------------------------
# Runtime callbacks: decode only the exact PC metadata and route all real
# actor creation through the atomic authority-aware spawn primitive.
# ---------------------------------------------------------------------------
callbacks = Path("src/runtime/callbacks.rs")
text = callbacks.read_text()
insert_anchor = "use std::sync::Arc;\n"
if text.count(insert_anchor) != 1:
    raise RuntimeError("callbacks import anchor moved")
helper = r'''

/// Spawn using authority metadata attached to the exact executing bytecode PC.
/// Any malformed metadata or parent escalation fails closed before a child is
/// created or enqueued.
fn spawn_with_site_authority(
    rt: &mut Runtime,
    module: &crate::bytecode::CodeModule,
    spawn_pc: usize,
    behavior_idx: usize,
    init: Vec<(String, crate::vm::Value)>,
) -> crate::vm::Value {
    let requested = match crate::authority_runtime::spawn_authority_manifest(module, spawn_pc) {
        Ok(manifest) => manifest,
        Err(error) => {
            tracing::warn!(
                spawn_pc,
                behavior_idx,
                %error,
                "refusing actor spawn with invalid authority metadata"
            );
            return crate::vm::Value::nil();
        }
    };

    match super::spawn::spawn_from_module_with_authority(
        rt,
        module,
        behavior_idx,
        init,
        &requested,
    ) {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(
                spawn_pc,
                behavior_idx,
                %error,
                "refusing actor spawn whose authority is not delegated by the parent"
            );
            crate::vm::Value::nil()
        }
    }
}
'''
text = text.replace(insert_anchor, insert_anchor + helper, 1)

sig_old = """        module: &crate::bytecode::CodeModule,\n        behavior_idx: usize,\n        init: Vec<(String, crate::vm::Value)>,"""
sig_new = """        module: &crate::bytecode::CodeModule,\n        spawn_pc: usize,\n        behavior_idx: usize,\n        init: Vec<(String, crate::vm::Value)>,"""
sig_count = text.count(sig_old)
if sig_count != 4:
    raise RuntimeError(f"callbacks: expected 4 spawn signatures, got {sig_count}")
text = text.replace(sig_old, sig_new)

rc_old = """        self.runtime\n            .borrow_mut()\n            .spawn_from_module(module, behavior_idx, init)"""
rc_new = """        let mut rt = self.runtime.borrow_mut();\n        spawn_with_site_authority(&mut rt, module, spawn_pc, behavior_idx, init)"""
rc_count = text.count(rc_old)
if rc_count != 2:
    raise RuntimeError(f"callbacks: expected 2 Rc spawn bodies, got {rc_count}")
text = text.replace(rc_old, rc_new)

raw_old = """        unsafe { (*self.runtime).spawn_from_module(module, behavior_idx, init) }"""
raw_new = """        unsafe {\n            spawn_with_site_authority(\n                &mut *self.runtime,\n                module,\n                spawn_pc,\n                behavior_idx,\n                init,\n            )\n        }"""
raw_count = text.count(raw_old)
if raw_count != 2:
    raise RuntimeError(f"callbacks: expected 2 raw-pointer spawn bodies, got {raw_count}")
text = text.replace(raw_old, raw_new)
callbacks.write_text(text)

# The exact-PC helpers are live after this patch; remove their staged dead-code
# allowances while leaving the temporary behavior-index compatibility bridge.
auth = Path("src/authority_runtime.rs")
auth_text = auth.read_text()
needle = """#[allow(dead_code)]\npub fn spawn_authority_manifest(\n"""
if auth_text.count(needle) != 1:
    raise RuntimeError("authority_runtime: spawn_authority_manifest marker moved")
auth_text = auth_text.replace(needle, "pub fn spawn_authority_manifest(\n", 1)
auth.write_text(auth_text)

spawn = Path("src/runtime/spawn.rs")
spawn_text = spawn.read_text()
needle = """#[allow(dead_code)]\npub(crate) fn spawn_from_module_with_authority(\n"""
if spawn_text.count(needle) != 1:
    raise RuntimeError("runtime/spawn: authority helper marker moved")
spawn_text = spawn_text.replace(needle, "pub(crate) fn spawn_from_module_with_authority(\n", 1)
spawn.write_text(spawn_text)

# ---------------------------------------------------------------------------
# Regression coverage: mutate lowered MIR directly so tests cover codegen and
# runtime provenance before source-level `spawn ... with [...]` syntax lands.
# ---------------------------------------------------------------------------
Path("tests/spawn_authority_provenance.rs").write_text(r'''use std::cell::RefCell;
use std::rc::Rc;

use nulang::authority_runtime::spawn_authority_manifest;
use nulang::bytecode::OpCode;
use nulang::lexer::Lexer;
use nulang::mir::{Module, RValue, Stmt};
use nulang::mir_codegen::compile_mir;
use nulang::parser::Parser;
use nulang::runtime::{Actor, Runtime, RuntimeVmCallbacks};
use nulang::typechecker::TypeChecker;
use nulang::vm::VM;

fn lower(source: &str) -> Module {
    let tokens = Lexer::new(source).lex().unwrap();
    let ast = Parser::new(tokens).parse_module().unwrap();
    let mut tc = TypeChecker::new();
    tc.check_module(&ast).unwrap();
    let hir = nulang::hir_lower::lower_module(&ast, &tc.inferred_decl_types);
    nulang::mir_lower::lower_module(&hir).unwrap()
}

fn set_spawn_grants(mir: &mut Module, site: usize, grants: &[&str]) {
    let mut seen = 0usize;
    for func in mir.functions.iter_mut().chain(mir.behaviors.iter_mut()) {
        for block in &mut func.blocks {
            for stmt in &mut block.stmts {
                if let Stmt::Assign {
                    op: RValue::Spawn { capabilities, .. },
                    ..
                } = stmt
                {
                    if seen == site {
                        *capabilities = grants.iter().map(|grant| (*grant).to_string()).collect();
                        return;
                    }
                    seen += 1;
                }
            }
        }
    }
    panic!("spawn site {site} not found (saw {seen})");
}

fn two_spawn_source() -> &'static str {
    r#"
actor Child {
    behavior ping() { 1 }
}
fn main() {
    let first = spawn Child {}
    let second = spawn Child {}
    second
}
"#
}

#[test]
fn codegen_records_distinct_grants_by_exact_spawn_pc() {
    let mut mir = lower(two_spawn_source());
    set_spawn_grants(&mut mir, 0, &["Secret::Read(FIRST_KEY)"]);
    set_spawn_grants(&mut mir, 1, &["Secret::Read(SECOND_KEY)"]);

    let module = compile_mir(&mut mir, "authority-pc").unwrap();
    let spawn_pcs: Vec<_> = module
        .instructions
        .iter()
        .enumerate()
        .filter_map(|(pc, instr)| (instr.opcode == OpCode::Spawn).then_some(pc))
        .collect();
    assert_eq!(spawn_pcs.len(), 2);
    assert_eq!(
        spawn_authority_manifest(&module, spawn_pcs[0])
            .unwrap()
            .canonical_tokens(),
        vec!["Secret::Read(FIRST_KEY)"]
    );
    assert_eq!(
        spawn_authority_manifest(&module, spawn_pcs[1])
            .unwrap()
            .canonical_tokens(),
        vec!["Secret::Read(SECOND_KEY)"]
    );
}

#[test]
fn empty_grants_emit_no_privilege_metadata() {
    let mut mir = lower(two_spawn_source());
    let module = compile_mir(&mut mir, "authority-empty").unwrap();
    assert!(module.spawn_capability_grants.is_empty());
}

#[test]
fn malformed_spawn_grant_fails_compilation() {
    let mut mir = lower(two_spawn_source());
    set_spawn_grants(&mut mir, 0, &["Net::TcpOut(api.example.com)"]);
    let error = compile_mir(&mut mir, "authority-invalid").unwrap_err();
    assert!(error.to_string().contains("invalid spawn authority grant"));
}

#[test]
fn privileged_remote_spawn_is_rejected_explicitly() {
    let mut mir = lower(
        r#"
actor Child { behavior ping() { 1 } }
fn main() {
    let node1 = 7
    spawn@node1 Child {}
}
"#,
    );
    set_spawn_grants(&mut mir, 0, &["Secret::Read(KEY)"]);
    let error = compile_mir(&mut mir, "authority-remote").unwrap_err();
    assert!(error
        .to_string()
        .contains("distributed spawn protocol carries typed authority"));
}

#[test]
fn vm_uses_exact_spawn_pc_even_for_same_target_behavior() {
    let mut mir = lower(two_spawn_source());
    set_spawn_grants(&mut mir, 0, &["Secret::Read(FIRST_KEY)"]);
    set_spawn_grants(&mut mir, 1, &["Secret::Read(SECOND_KEY)"]);
    let module = compile_mir(&mut mir, "authority-vm-pc").unwrap();

    let runtime = Rc::new(RefCell::new(Runtime::new()));
    let mut vm = VM::new();
    vm.load_module(module);
    vm.set_actor_callbacks(Box::new(RuntimeVmCallbacks::new(runtime.clone())));
    vm.run().unwrap();

    let mut manifests: Vec<Vec<String>> = runtime
        .borrow()
        .actors
        .values()
        .map(|actor| actor.authority_manifest().unwrap().canonical_tokens())
        .collect();
    manifests.sort();
    assert_eq!(
        manifests,
        vec![
            vec!["Secret::Read(FIRST_KEY)".to_string()],
            vec!["Secret::Read(SECOND_KEY)".to_string()],
        ]
    );
}

#[test]
fn parent_escalation_is_rejected_before_child_creation() {
    let mut mir = lower(
        r#"
actor Child { behavior ping() { 1 } }
fn main() { spawn Child {} }
"#,
    );
    set_spawn_grants(&mut mir, 0, &["Secret::Read(UNHELD_KEY)"]);
    let module = compile_mir(&mut mir, "authority-attenuation").unwrap();

    let runtime = Rc::new(RefCell::new(Runtime::new()));
    {
        let mut rt = runtime.borrow_mut();
        let parent_id = 900_001;
        rt.actors
            .insert(parent_id, Actor::new(parent_id, "unprivileged-parent", 0));
        rt.current_actor = Some(parent_id);
    }

    let mut vm = VM::new();
    vm.load_module(module);
    vm.set_actor_callbacks(Box::new(RuntimeVmCallbacks::new(runtime.clone())));
    let result = vm.run().unwrap();
    assert!(result.is_nil());

    let rt = runtime.borrow();
    assert_eq!(rt.actors.len(), 1, "denied spawn must not create a child");
    assert!(rt.actors.contains_key(&900_001));
}
''')

print("spawn-authority provenance patch applied")
