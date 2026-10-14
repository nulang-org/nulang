//! Pure typed-actor bytecode may stage scalar state in an isolated VM and
//! commit only through the existing atomic admitted-result boundary.
use nulang::bytecode::{CodeModule, Instruction, OpCode};
use nulang::lexer::Lexer;
use nulang::parser::Parser;
use nulang::runtime::{
    DurableTransition, JournalEntry, PersistedValue, PersistenceStore, Runtime,
    DURABLE_TRANSITION_VERSION,
};
use nulang::typechecker::TypeChecker;
use nulang::vm::Value;

fn compiled_counter() -> CodeModule {
    let tokens = Lexer::new(
        r#"
        persistent actor Counter {
            state durable total: Int = 0
            behavior add(by: Int) { self.total = self.total + by }
        }
        "#,
    )
    .lex()
    .expect("lex");
    let ast = Parser::new(tokens).parse_module().expect("parse");
    let mut checker = TypeChecker::new();
    checker.check_module(&ast).expect("typecheck");
    let hir = nulang::hir_lower::lower_module(&ast, &checker.inferred_decl_types);
    let mut mir = nulang::mir_lower::lower_module(&hir).expect("MIR");
    nulang::mir_codegen::compile_mir(&mut mir, "staged_pure_turn").expect("codegen")
}

fn spawned(module: &CodeModule) -> (Runtime, u64, u16) {
    let behavior = module.actor_metadata[0].behavior_indices[0] as u16;
    let mut runtime = Runtime::new();
    let actor_id = runtime
        .spawn_from_module(module, behavior as usize, vec![])
        .as_actor_id()
        .expect("typed persistent actor");
    (runtime, actor_id, behavior)
}

fn admit(runtime: &mut Runtime, actor_id: u64, behavior: u16, amount: i64) -> u64 {
    runtime.send_message_by_id(actor_id, behavior, &[Value::int(amount)]);
    let predecessor = runtime.persistence.latest_sequence(actor_id);
    let epoch = runtime.actors[&actor_id].activation_epoch;
    runtime
        .admit_next_typed_actor_command(
            actor_id,
            DurableTransition {
                version: DURABLE_TRANSITION_VERSION,
                actor_id,
                activation_epoch: epoch,
                sequence: predecessor + 1,
                expected_previous_sequence: predecessor,
                command: Some(JournalEntry {
                    sequence: predecessor + 1,
                    behavior_id: behavior,
                    payload: vec![PersistedValue::Int(amount)],
                }),
                snapshot: None,
                workflow_events: vec![],
                domain_events: vec![],
                durable_effects: vec![],
                outbox: vec![],
            },
        )
        .expect("persist admission")
        .expect("admitted pending message")
        .1
        .sequence
}

#[test]
fn compiled_scalar_handler_stages_then_atomically_publishes_the_result() {
    let (mut runtime, actor_id, behavior) = spawned(&compiled_counter());
    let sequence = admit(&mut runtime, actor_id, behavior, 5);
    assert_eq!(
        runtime.actors[&actor_id]
            .get_state_field("total")
            .and_then(|value| value.as_int()),
        Some(0),
        "admission itself must never execute the handler"
    );

    let completed = runtime
        .execute_and_commit_admitted_pure_typed_actor_turn(actor_id, sequence)
        .expect("pure scalar handler can complete atomically");
    assert_eq!(completed.sequence, sequence + 1);
    assert_eq!(runtime.persistence.latest_sequence(actor_id), sequence + 1);
    assert_eq!(runtime.persistence.read_journal(actor_id).len(), 1);
    assert_eq!(
        runtime.actors[&actor_id]
            .get_state_field("total")
            .and_then(|value| value.as_int()),
        Some(5)
    );
    assert_eq!(
        runtime
            .persistence
            .load_snapshot(actor_id)
            .unwrap()
            .state
            .get("total"),
        Some(&PersistedValue::Int(5))
    );
}

#[test]
fn unsafe_opcode_is_rejected_without_running_user_bytecode_or_releasing_fence() {
    let mut module = compiled_counter();
    let index = module.actor_metadata[0].behavior_indices[0];
    let offset = module.behaviors[index].code_offset;
    module.instructions[offset] = Instruction::new1(OpCode::Print, 0);
    let (mut runtime, actor_id, behavior) = spawned(&module);
    let sequence = admit(&mut runtime, actor_id, behavior, 5);
    let error = runtime
        .execute_and_commit_admitted_pure_typed_actor_turn(actor_id, sequence)
        .expect_err("host I/O instruction is forbidden even if later code is pure");
    assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
    assert_eq!(runtime.persistence.latest_sequence(actor_id), sequence);
    assert_eq!(
        runtime.actors[&actor_id].mailbox.admitted_command_sequence(),
        Some(sequence)
    );
    assert_eq!(
        runtime.actors[&actor_id]
            .get_state_field("total")
            .and_then(|value| value.as_int()),
        Some(0)
    );
}

#[test]
fn compiler_owned_state_boundary_rejects_an_unknown_written_field() {
    let mut module = compiled_counter();
    let index = module.actor_metadata[0].behavior_indices[0];
    let offset = module.behaviors[index].code_offset;
    let name_index = module.constants.len();
    module.constants.push(nulang::bytecode::Constant::String(
        "unowned_state".to_string(),
    ));
    module.instructions[offset] = Instruction::new3(
        OpCode::StateSet,
        (name_index >> 8) as u8,
        name_index as u8,
        0,
    );
    let (mut runtime, actor_id, behavior) = spawned(&module);
    let sequence = admit(&mut runtime, actor_id, behavior, 5);
    let error = runtime
        .execute_and_commit_admitted_pure_typed_actor_turn(actor_id, sequence)
        .expect_err("unowned state write must fail during bytecode preflight");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(runtime.persistence.latest_sequence(actor_id), sequence);
    assert!(!runtime.actors[&actor_id].state_data.contains_key("unowned_state"));
}

#[test]
fn execution_without_an_admitted_command_is_rejected() {
    let (mut runtime, actor_id, _) = spawned(&compiled_counter());
    let error = runtime
        .execute_and_commit_admitted_pure_typed_actor_turn(actor_id, 1)
        .expect_err("only the exact admitted message may run");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(runtime.persistence.latest_sequence(actor_id), 0);
}

#[test]
fn rejected_scalar_type_change_never_commits_or_updates_live_state() {
    let mut module = compiled_counter();
    let index = module.actor_metadata[0].behavior_indices[0];
    let offset = module.behaviors[index].code_offset;
    assert!(module.instructions.len() >= offset + 3);

    let bool_index = module.constants.len();
    module.constants.push(nulang::bytecode::Constant::Bool(true));
    let field_index = module.constants.len();
    module.constants.push(nulang::bytecode::Constant::String(
        "total".to_string(),
    ));
    module.instructions[offset] = Instruction::new3(
        OpCode::ConstU,
        (bool_index >> 8) as u8,
        bool_index as u8,
        0,
    );
    module.instructions[offset + 1] = Instruction::new3(
        OpCode::StateSet,
        (field_index >> 8) as u8,
        field_index as u8,
        0,
    );
    module.instructions[offset + 2] = Instruction::new0(OpCode::Ret);

    let (mut runtime, actor_id, behavior) = spawned(&module);
    let sequence = admit(&mut runtime, actor_id, behavior, 5);
    let error = runtime
        .execute_and_commit_admitted_pure_typed_actor_turn(actor_id, sequence)
        .expect_err("pure staged state must retain its compiler-owned scalar type");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert_eq!(runtime.persistence.latest_sequence(actor_id), sequence);
    assert_eq!(
        runtime.actors[&actor_id].mailbox.admitted_command_sequence(),
        Some(sequence)
    );
    assert_eq!(
        runtime.actors[&actor_id]
            .get_state_field("total")
            .and_then(|value| value.as_int()),
        Some(0)
    );
}

#[test]
fn early_halt_is_not_a_valid_completed_durable_handler() {
    let mut module = compiled_counter();
    let index = module.actor_metadata[0].behavior_indices[0];
    let offset = module.behaviors[index].code_offset;
    module.instructions[offset] = Instruction::new0(OpCode::Halt);

    let (mut runtime, actor_id, behavior) = spawned(&module);
    let sequence = admit(&mut runtime, actor_id, behavior, 5);
    let error = runtime
        .execute_and_commit_admitted_pure_typed_actor_turn(actor_id, sequence)
        .expect_err("an early halt must not count as a completed behavior");
    assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
    assert_eq!(runtime.persistence.latest_sequence(actor_id), sequence);
    assert_eq!(
        runtime.actors[&actor_id].mailbox.admitted_command_sequence(),
        Some(sequence)
    );
    assert_eq!(
        runtime.actors[&actor_id]
            .get_state_field("total")
            .and_then(|value| value.as_int()),
        Some(0)
    );
}

#[derive(Default)]
struct RejectResultStore {
    inner: nulang::runtime::MemoryStore,
    reject_result: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl nulang::runtime::PersistenceStore for RejectResultStore {
    fn commit_transition(
        &mut self,
        transition: DurableTransition,
    ) -> std::io::Result<nulang::runtime::DurableCommit> {
        if transition.snapshot.is_some()
            && self
                .reject_result
                .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Err(std::io::Error::other("injected result commit failure"));
        }
        self.inner.commit_transition(transition)
    }

    fn load_durable_tail_position(
        &self,
        actor_id: u64,
    ) -> std::io::Result<Option<nulang::runtime::DurableTailPosition>> {
        self.inner.load_durable_tail_position(actor_id)
    }

    fn save_snapshot(&mut self, snapshot: nulang::runtime::ActorSnapshot) -> std::io::Result<()> {
        self.inner.save_snapshot(snapshot)
    }

    fn load_snapshot(&self, actor_id: u64) -> Option<nulang::runtime::ActorSnapshot> {
        self.inner.load_snapshot(actor_id)
    }

    fn append_journal(&mut self, actor_id: u64, entry: JournalEntry) -> std::io::Result<()> {
        self.inner.append_journal(actor_id, entry)
    }

    fn read_journal(&self, actor_id: u64) -> Vec<JournalEntry> {
        self.inner.read_journal(actor_id)
    }

    fn append_workflow_event(
        &mut self,
        actor_id: u64,
        event: nulang::runtime::WorkflowEvent,
    ) -> std::io::Result<()> {
        self.inner.append_workflow_event(actor_id, event)
    }

    fn read_workflow_events(&self, actor_id: u64) -> Vec<nulang::runtime::WorkflowEvent> {
        self.inner.read_workflow_events(actor_id)
    }

    fn append_event(
        &mut self,
        actor_id: u64,
        event: nulang::runtime::EventEntry,
    ) -> std::io::Result<()> {
        self.inner.append_event(actor_id, event)
    }

    fn read_events(&self, actor_id: u64) -> Vec<nulang::runtime::EventEntry> {
        self.inner.read_events(actor_id)
    }

    fn latest_sequence(&self, actor_id: u64) -> u64 {
        self.inner.latest_sequence(actor_id)
    }

    fn clear(&mut self, actor_id: u64) -> std::io::Result<()> {
        self.inner.clear(actor_id)
    }
}

#[test]
fn failed_atomic_result_commit_keeps_live_state_fenced_until_retry_succeeds() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let (mut runtime, actor_id, behavior) = spawned(&compiled_counter());
    let reject_result = Arc::new(AtomicBool::new(true));
    runtime.persistence = Box::new(RejectResultStore {
        inner: nulang::runtime::MemoryStore::default(),
        reject_result: Arc::clone(&reject_result),
    });
    let sequence = admit(&mut runtime, actor_id, behavior, 5);
    let error = runtime
        .execute_and_commit_admitted_pure_typed_actor_turn(actor_id, sequence)
        .expect_err("persisted state must commit before any live publication");
    assert_eq!(error.kind(), std::io::ErrorKind::Other);
    assert_eq!(runtime.persistence.latest_sequence(actor_id), sequence);
    assert!(runtime.persistence.load_snapshot(actor_id).is_none());
    assert_eq!(
        runtime.actors[&actor_id].mailbox.admitted_command_sequence(),
        Some(sequence)
    );
    assert_eq!(
        runtime.actors[&actor_id]
            .get_state_field("total")
            .and_then(|value| value.as_int()),
        Some(0)
    );

    reject_result.store(false, Ordering::SeqCst);
    let result = runtime
        .execute_and_commit_admitted_pure_typed_actor_turn(actor_id, sequence)
        .expect("same admitted command safely retries after storage recovers");
    assert_eq!(result.sequence, sequence + 1);
    assert_eq!(runtime.persistence.read_journal(actor_id).len(), 1);
    assert_eq!(
        runtime.actors[&actor_id]
            .get_state_field("total")
            .and_then(|value| value.as_int()),
        Some(5)
    );
}
