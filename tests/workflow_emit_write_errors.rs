//! Failed durable journal writes must abort an activation, never masquerade as
//! a successfully emitted output or consume its replay identity.
use nulang::runtime::{
    JsonFileStore, PersistedValue, Runtime, WorkflowActivationId, WorkflowEvent,
    WorkflowReplayEventId,
};
use nulang::vm::{ActorVmCallbacks, Value};
use std::collections::HashMap;
use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

fn fresh_workflow() -> (Runtime, u64) {
    let mut rt = Runtime::new();
    let id = rt.spawn_workflow_actor("Output", Box::new(Vec::new), HashMap::new());
    assert_ne!(id, 0);
    (rt, id)
}

fn temporary_store() -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "nulang-workflow-emit-failure-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
    ))
}

#[test]
fn workflow_emit_append_failure_returns_error_without_mutating_actor_log() {
    let (mut rt, id) = fresh_workflow();
    let dir = temporary_store();
    let actor_dir = dir.join(format!("actor_{id}"));
    fs::create_dir_all(actor_dir.join("workflow_events.jsonl")).unwrap();
    rt.persistence = Box::new(JsonFileStore::new(&dir).unwrap());
    let count = rt.actors[&id].event_log.len();
    let result = rt.try_emit_event(id, "WorkflowOutput", &[Value::int(7)]);
    assert!(
        result.is_err(),
        "failed append must reject the workflow output"
    );
    assert_eq!(
        rt.actors[&id].event_log.len(),
        count,
        "failed persistence must not publish in-memory output"
    );
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn workflow_emit_replay_conflict_returns_error_preserving_ordinal() {
    let (mut rt, id) = fresh_workflow();
    let activation = WorkflowActivationId::new(id, 93);
    rt.actors.get_mut(&id).unwrap().current_workflow_activation = Some(activation);
    let replay_id = WorkflowReplayEventId::new(activation, 0);
    rt.persistence
        .append_workflow_event(
            id,
            WorkflowEvent::Custom {
                sequence: rt.persistence.latest_sequence(id) + 1,
                replay_id: Some(replay_id),
                name: "WorkflowOutput".into(),
                args: vec![PersistedValue::String("first".into())],
            },
        )
        .unwrap();
    let err = rt
        .try_emit_event(id, "WorkflowOutput", &[Value::int(3)])
        .expect_err("same replay identity with distinct payload must error");
    assert!(err.to_string().contains("conflict"), "{err}");
    assert_eq!(rt.actors[&id].workflow_replay_event_ordinal, 0);
    assert!(
        rt.actors[&id].event_log.is_empty(),
        "conflict must not publish output"
    );
}

#[test]
fn workflow_emit_success_is_visible_in_journal_after_commit() {
    let (mut rt, id) = fresh_workflow();
    let activation = WorkflowActivationId::new(id, 94);
    rt.actors.get_mut(&id).unwrap().current_workflow_activation = Some(activation);
    rt.try_emit_event(id, "WorkflowOutput", &[Value::int(4)])
        .unwrap();
    assert_eq!(rt.actors[&id].workflow_replay_event_ordinal, 1);
    let events = rt.persistence.read_workflow_events(id);
    assert!(events.iter().any(|entry| matches!(entry, WorkflowEvent::Custom { name, replay_id: Some(_), .. } if name == "WorkflowOutput")));
    assert_eq!(rt.actors[&id].event_log.len(), 1);
}

fn broken_runtime() -> (
    std::rc::Rc<std::cell::RefCell<Runtime>>,
    u64,
    std::path::PathBuf,
) {
    let (mut runtime, actor_id) = fresh_workflow();
    let dir = temporary_store();
    fs::create_dir_all(dir.join(format!("actor_{actor_id}/workflow_events.jsonl"))).unwrap();
    runtime.persistence = Box::new(JsonFileStore::new(&dir).unwrap());
    runtime.current_actor = Some(actor_id);
    (
        std::rc::Rc::new(std::cell::RefCell::new(runtime)),
        actor_id,
        dir,
    )
}

#[test]
fn interpreter_emit_opcode_aborts_on_durable_append_failure() {
    use nulang::hir_lower;
    use nulang::lexer::Lexer;
    use nulang::mir_codegen;
    use nulang::mir_lower;
    use nulang::parser::Parser;
    use nulang::runtime::RuntimeVmCallbacks;
    use nulang::typechecker::TypeChecker;
    use nulang::vm::VM;

    let source = "fn main() { emit WorkflowOutput() }";
    let tokens = Lexer::new(source).lex().unwrap();
    let ast = Parser::new(tokens).parse_module().unwrap();
    let mut tc = TypeChecker::new();
    tc.check_module(&ast).unwrap();
    let hir = hir_lower::lower_module(&ast, &tc.inferred_decl_types);
    let mut mir = mir_lower::lower_module(&hir).unwrap();
    let compiled = mir_codegen::compile_mir(&mut mir, "write-failure").unwrap();

    let (runtime, actor_id, dir) = broken_runtime();
    let mut vm = VM::new();
    vm.load_module(compiled);
    vm.set_actor_callbacks(Box::new(RuntimeVmCallbacks::new(runtime.clone())));
    let err = vm
        .run()
        .expect_err("Emit opcode must propagate append error");
    assert!(err.to_string().contains("durable emit"), "{err}");
    assert!(runtime.borrow().actors[&actor_id].event_log.is_empty());
    fs::remove_dir_all(dir).unwrap();
}

#[cfg(feature = "native-codegen")]
#[test]
fn native_emit_helper_returns_failure_status_and_pending_error() {
    use nulang::bytecode::Constant;
    use nulang::jit::runtime::{
        aot_clear_constants, aot_set_constants, aot_take_pending_error, clear_jit_callbacks,
        nulang_aot_emit_0, set_jit_callbacks,
    };
    use nulang::runtime::RuntimeVmCallbacks;
    let (runtime, actor_id, dir) = broken_runtime();
    let mut callbacks = RuntimeVmCallbacks::new(runtime.clone());
    let trait_callbacks: &mut dyn ActorVmCallbacks = &mut callbacks;
    unsafe {
        aot_set_constants(&[Constant::String("WorkflowOutput".into())]);
        set_jit_callbacks(trait_callbacks as *mut dyn ActorVmCallbacks);
        let status = nulang_aot_emit_0(Value::string(0).as_raw());
        clear_jit_callbacks();
        aot_clear_constants();
        assert_eq!(status, 0, "native emit helper must signal failure");
    }
    let error = aot_take_pending_error().expect("AOT driver must observe a durable emit error");
    assert!(error.contains("durable emit"), "{error}");
    assert!(runtime.borrow().actors[&actor_id].event_log.is_empty());
    fs::remove_dir_all(dir).unwrap();
}

#[cfg(feature = "native-codegen")]
#[test]
fn native_codegen_compiles_abortive_emit_status_branches() {
    use nulang::effect_checker::EffectChecker;
    use nulang::hir_lower;
    use nulang::lexer::Lexer;
    use nulang::mir_lower;
    use nulang::parser::Parser;
    use nulang::typechecker::TypeChecker;

    // Both emits would execute without the native status-checked early return.
    // Cranelift must generate valid code for the success/failure split rather
    // than keeping the unsafe, void-returning helper ABI.
    let source = "actor Sender { behavior fire() { emit First(); emit Second() } } fn main() { 0 }";
    let tokens = Lexer::new(source).lex().unwrap();
    let ast = Parser::new(tokens).parse_module().unwrap();
    let mut tc = TypeChecker::new();
    tc.check_module(&ast).unwrap();
    EffectChecker::new().check_module(&ast.decls).unwrap();
    let hir = hir_lower::lower_module(&ast, &tc.inferred_decl_types);
    let mir = mir_lower::lower_module(&hir).unwrap();
    nulang::aot::AotModule::compile(&mir).expect("native Emit control flow must compile");
}
