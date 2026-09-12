use std::cell::RefCell;
use std::rc::Rc;

use nulang::aot::AotModule;
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

fn spawn_grants(module: &nulang::bytecode::CodeModule, pc: usize) -> Vec<String> {
    module
        .spawn_capability_grants
        .iter()
        .find(|(site, _)| *site == pc)
        .map(|(_, grants)| grants.clone())
        .unwrap_or_default()
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
fn source_grants_survive_frontend_and_bind_to_exact_spawn_pc() {
    let mut mir = lower(
        r#"
actor Child {
    behavior ping() { 1 }
}
fn main() {
    let first = spawn Child {} with [Secret::Read("FIRST_KEY")]
    let second = spawn Child {} with [Secret::Read("SECOND_KEY")]
    second
}
"#,
    );
    let module = compile_mir(&mut mir, "authority-source-pc").unwrap();
    let spawn_pcs: Vec<_> = module
        .instructions
        .iter()
        .enumerate()
        .filter_map(|(pc, instr)| (instr.opcode == OpCode::Spawn).then_some(pc))
        .collect();
    assert_eq!(spawn_pcs.len(), 2);
    assert_eq!(
        spawn_grants(&module, spawn_pcs[0]),
        vec!["Secret::Read(FIRST_KEY)"]
    );
    assert_eq!(
        spawn_grants(&module, spawn_pcs[1]),
        vec!["Secret::Read(SECOND_KEY)"]
    );
}

#[test]
fn source_grants_are_canonicalized_and_deduplicated() {
    let mut mir = lower(
        r#"
actor Child { behavior ping() { 1 } }
fn main() {
    spawn Child {} with [
        Secret::Read("B"),
        Fs::Read("/tmp/input"),
        Secret::Read("B"),
        Env::Read("HOME")
    ]
}
"#,
    );
    let module = compile_mir(&mut mir, "authority-source-canonical").unwrap();
    let spawn_pc = module
        .instructions
        .iter()
        .position(|instr| instr.opcode == OpCode::Spawn)
        .unwrap();
    assert_eq!(
        spawn_grants(&module, spawn_pc),
        vec!["Env::Read(HOME)", "Fs::Read(/tmp/input)", "Secret::Read(B)",]
    );
}

#[test]
fn malformed_source_grant_fails_during_parse() {
    let source = r#"
actor Child { behavior ping() { 1 } }
fn main() {
    spawn Child {} with [Net::TcpOut("api.example.com")]
}
"#;
    let tokens = Lexer::new(source).lex().unwrap();
    let error = Parser::new(tokens).parse_module().unwrap_err();
    assert!(error.to_string().contains("TcpOut expects host:port"));
}

#[test]
fn dynamic_source_grant_argument_is_rejected() {
    let source = r#"
actor Child { behavior ping() { 1 } }
fn main() {
    let key = "KEY"
    spawn Child {} with [Secret::Read(key)]
}
"#;
    let tokens = Lexer::new(source).lex().unwrap();
    assert!(Parser::new(tokens).parse_module().is_err());
}

#[test]
fn privileged_remote_spawn_from_source_fails_closed() {
    let mut mir = lower(
        r#"
actor Child { behavior ping() { 1 } }
fn main() {
    let node1 = 7
    spawn@node1 Child {} with [Secret::Read("KEY")]
}
"#,
    );
    let error = compile_mir(&mut mir, "authority-source-remote").unwrap_err();
    assert!(error
        .to_string()
        .contains("distributed spawn protocol carries typed authority"));
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
        spawn_grants(&module, spawn_pcs[0]),
        vec!["Secret::Read(FIRST_KEY)"]
    );
    assert_eq!(
        spawn_grants(&module, spawn_pcs[1]),
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
