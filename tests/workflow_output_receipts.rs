//! Durable workflow output is a journal record, not a print side effect.
//! Readers must reject ambiguous replay identity rather than guessing.
use nulang::runtime::{
    MemoryStore, PersistedValue, PersistenceStore, WorkflowActivationId, WorkflowEvent,
    WorkflowReplayEventId,
};
use nulang::workflow_output::{read_workflow_output_receipts, WorkflowOutputError};

const ACTOR: u64 = 42;

fn identity(command_sequence: u64, ordinal: u32) -> WorkflowReplayEventId {
    WorkflowReplayEventId::new(WorkflowActivationId::new(ACTOR, command_sequence), ordinal)
}

fn output(sequence: u64, replay_id: Option<WorkflowReplayEventId>, text: &str) -> WorkflowEvent {
    WorkflowEvent::Custom {
        sequence,
        replay_id,
        name: "WorkflowOutput".into(),
        args: vec![PersistedValue::String(text.into())],
    }
}

#[test]
fn output_receipts_are_stable_across_readers_and_pages() {
    let mut store = MemoryStore::new();
    store
        .append_workflow_event(ACTOR, output(1, Some(identity(100, 0)), "reserved"))
        .unwrap();
    store
        .append_workflow_event(
            ACTOR,
            WorkflowEvent::StepCompleted {
                sequence: 2,
                activation: Some(WorkflowActivationId::new(ACTOR, 100)),
                step_name: "reserve".into(),
            },
        )
        .unwrap();
    store
        .append_workflow_event(ACTOR, output(3, Some(identity(101, 0)), "charged"))
        .unwrap();
    let first = read_workflow_output_receipts(&store, ACTOR, 0, 1).unwrap();
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].sequence, 1);
    assert_eq!(first[0].value, PersistedValue::String("reserved".into()));
    assert_eq!(first[0].replay_id, identity(100, 0));
    let second = read_workflow_output_receipts(&store, ACTOR, 1, 1).unwrap();
    assert_eq!(second.len(), 1);
    assert_eq!(second[0].value, PersistedValue::String("charged".into()));
    assert_ne!(first[0].idempotency_key(), second[0].idempotency_key());
    assert_eq!(
        read_workflow_output_receipts(&store, ACTOR, 0, 2).unwrap(),
        [first, second].concat()
    );
}

#[test]
fn rejects_legacy_output_without_replay_identity() {
    let mut store = MemoryStore::new();
    store
        .append_workflow_event(ACTOR, output(1, None, "unsafe"))
        .unwrap();
    assert!(matches!(
        read_workflow_output_receipts(&store, ACTOR, 0, 10),
        Err(WorkflowOutputError::MissingReplayIdentity { .. })
    ));
}

#[test]
fn rejects_conflicting_duplicate_replay_identity_even_when_paginating() {
    let mut store = MemoryStore::new();
    store
        .append_workflow_event(ACTOR, output(1, Some(identity(100, 0)), "a"))
        .unwrap();
    store
        .append_workflow_event(ACTOR, output(2, Some(identity(100, 0)), "b"))
        .unwrap();
    assert!(matches!(
        read_workflow_output_receipts(&store, ACTOR, 0, 1),
        Err(WorkflowOutputError::DuplicateReplayIdentity { .. })
    ));
}

#[test]
fn rejects_timer_output_replay_identity_collision() {
    let mut store = MemoryStore::new();
    store
        .append_workflow_event(
            ACTOR,
            WorkflowEvent::TimerSet {
                sequence: 1,
                replay_id: Some(identity(100, 0)),
                name: "timeout".into(),
                duration_ms: 100,
            },
        )
        .unwrap();
    store
        .append_workflow_event(ACTOR, output(2, Some(identity(100, 0)), "bad"))
        .unwrap();
    assert!(matches!(
        read_workflow_output_receipts(&store, ACTOR, 0, 10),
        Err(WorkflowOutputError::DuplicateReplayIdentity { .. })
    ));
}

#[test]
fn rejects_wrong_actor_and_invalid_payload() {
    let mut store = MemoryStore::new();
    store
        .append_workflow_event(
            ACTOR,
            output(
                1,
                Some(WorkflowReplayEventId::new(
                    WorkflowActivationId::new(99, 100),
                    0,
                )),
                "wrong",
            ),
        )
        .unwrap();
    assert!(matches!(
        read_workflow_output_receipts(&store, ACTOR, 0, 10),
        Err(WorkflowOutputError::WrongActor { .. })
    ));
    let mut store = MemoryStore::new();
    store
        .append_workflow_event(
            ACTOR,
            WorkflowEvent::Custom {
                sequence: 1,
                replay_id: Some(identity(100, 0)),
                name: "WorkflowOutput".into(),
                args: vec![PersistedValue::Int(5)],
            },
        )
        .unwrap();
    assert!(matches!(
        read_workflow_output_receipts(&store, ACTOR, 0, 10),
        Err(WorkflowOutputError::InvalidOutputPayload { .. })
    ));
}

#[test]
fn rejects_duplicate_or_out_of_order_journal_sequence() {
    let mut store = MemoryStore::new();
    store
        .append_workflow_event(ACTOR, output(3, Some(identity(100, 0)), "a"))
        .unwrap();
    store
        .append_workflow_event(ACTOR, output(2, Some(identity(100, 1)), "b"))
        .unwrap();
    assert!(matches!(
        read_workflow_output_receipts(&store, ACTOR, 0, 10),
        Err(WorkflowOutputError::InvalidSequence { .. })
    ));
}

#[test]
fn empty_page_size_is_an_error() {
    let store = MemoryStore::new();
    assert!(matches!(
        read_workflow_output_receipts(&store, ACTOR, 0, 0),
        Err(WorkflowOutputError::InvalidLimit)
    ));
}

#[test]
fn workflow_emit_output_is_journaled_and_readable_after_runtime_restart() {
    use nulang::hir_lower;
    use nulang::lexer::Lexer;
    use nulang::mir_codegen;
    use nulang::mir_lower;
    use nulang::parser::Parser;
    use nulang::runtime::{MemoryStore, Runtime, RuntimeVmCallbacks};
    use nulang::typechecker::TypeChecker;
    use nulang::vm::VM;
    use std::cell::RefCell;
    use std::rc::Rc;

    let source = "workflow W { step record { emit WorkflowOutput(\"reserved\") } step charge { emit WorkflowOutput(\"charged\") } step ship { emit WorkflowOutput(\"shipped\") } } spawn W {}";
    let tokens = Lexer::new(source).lex().unwrap();
    let ast = Parser::new(tokens).parse_module().unwrap();
    let mut tc = TypeChecker::new();
    tc.check_module(&ast).unwrap();
    let hir = hir_lower::lower_module(&ast, &tc.inferred_decl_types);
    let mut mir = mir_lower::lower_module(&hir).unwrap();
    let module = mir_codegen::compile_mir(&mut mir, "workflow-output-receipt").unwrap();

    let runtime = Rc::new(RefCell::new(Runtime::new()));
    let actor_id = {
        let mut vm = VM::new();
        vm.load_module(module.clone());
        vm.set_actor_callbacks(Box::new(RuntimeVmCallbacks::new(runtime.clone())));
        vm.run().unwrap().as_actor_id().unwrap()
    };
    for behavior_id in 0..3 {
        runtime
            .borrow_mut()
            .send_message_by_id(actor_id, behavior_id, &[]);
        runtime.borrow_mut().run_scheduler();
    }
    let receipts =
        read_workflow_output_receipts(&*runtime.borrow().persistence, actor_id, 0, 10).unwrap();
    assert_eq!(
        receipts.len(),
        3,
        "three workflow steps should produce three journal receipts"
    );
    assert_eq!(receipts[0].value, PersistedValue::String("reserved".into()));
    assert_eq!(receipts[1].value, PersistedValue::String("charged".into()));
    assert_eq!(receipts[2].value, PersistedValue::String("shipped".into()));
    assert_eq!(receipts[0].replay_id.activation.actor_id, actor_id);
    assert!(receipts
        .windows(2)
        .all(|pair| pair[0].sequence < pair[1].sequence));
    assert!(receipts
        .windows(2)
        .all(|pair| pair[0].idempotency_key() != pair[1].idempotency_key()));

    // Move the same persisted backing store to a fresh runtime to simulate
    // recovery. This is a runtime-object restart, not a process-kill test.
    let persisted = std::mem::replace(
        &mut runtime.borrow_mut().persistence,
        Box::new(MemoryStore::new()),
    );
    let recovered = Rc::new(RefCell::new(Runtime::new()));
    recovered.borrow_mut().persistence = persisted;
    let mut offsets = vec![0; module.behaviors.len()];
    if let Some(meta) = module.actor_metadata.first() {
        for &idx in &meta.behavior_indices {
            offsets[idx] = module.behaviors[idx].code_offset;
        }
    }
    recovered.borrow_mut().register_recovery_module(
        actor_id,
        module.clone(),
        offsets,
        vec![None; module.behaviors.len()],
    );
    recovered.borrow_mut().recover_actor(actor_id);
    let after =
        read_workflow_output_receipts(&*recovered.borrow().persistence, actor_id, 0, 10).unwrap();
    assert_eq!(
        after, receipts,
        "restarting the runtime must not duplicate receipts"
    );
}
