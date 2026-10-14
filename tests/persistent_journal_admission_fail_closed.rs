//! Refuse to execute persistent actor messages when journal admission fails.
//! A transient actor must never consult that journal for its ordinary turns.

use std::io;

use nulang::lexer::Lexer;
use nulang::parser::Parser;
use nulang::runtime::{
    Actor, ActorSnapshot, EventEntry, JournalEntry, MemoryStore, PersistenceStore, Runtime,
    WorkflowEvent,
};
use nulang::typechecker::TypeChecker;
use nulang::vm::Value;

/// The backend can persist initial snapshots but loses the ability to admit
/// commands. A successful handler would mutate unjournaled state.
#[derive(Default)]
struct RejectJournalStore {
    inner: MemoryStore,
}

impl PersistenceStore for RejectJournalStore {
    fn save_snapshot(&mut self, snapshot: ActorSnapshot) -> io::Result<()> {
        self.inner.save_snapshot(snapshot)
    }

    fn load_snapshot(&self, actor_id: u64) -> Option<ActorSnapshot> {
        self.inner.load_snapshot(actor_id)
    }

    fn append_journal(&mut self, _actor_id: u64, _entry: JournalEntry) -> io::Result<()> {
        Err(io::Error::other("injected durable admission failure"))
    }

    fn read_journal(&self, actor_id: u64) -> Vec<JournalEntry> {
        self.inner.read_journal(actor_id)
    }

    fn append_workflow_event(&mut self, actor_id: u64, event: WorkflowEvent) -> io::Result<()> {
        self.inner.append_workflow_event(actor_id, event)
    }

    fn read_workflow_events(&self, actor_id: u64) -> Vec<WorkflowEvent> {
        self.inner.read_workflow_events(actor_id)
    }

    fn append_event(&mut self, actor_id: u64, event: EventEntry) -> io::Result<()> {
        self.inner.append_event(actor_id, event)
    }

    fn read_events(&self, actor_id: u64) -> Vec<EventEntry> {
        self.inner.read_events(actor_id)
    }

    fn latest_sequence(&self, actor_id: u64) -> u64 {
        self.inner.latest_sequence(actor_id)
    }

    fn clear(&mut self, actor_id: u64) -> io::Result<()> {
        self.inner.clear(actor_id)
    }
}

fn compile(source: &str) -> nulang::bytecode::CodeModule {
    let tokens = Lexer::new(source).lex().expect("lex");
    let ast = Parser::new(tokens).parse_module().expect("parse");
    let mut checker = TypeChecker::new();
    checker.check_module(&ast).expect("typecheck");
    let hir = nulang::hir_lower::lower_module(&ast, &checker.inferred_decl_types);
    let mut mir = nulang::mir_lower::lower_module(&hir).expect("MIR lower");
    nulang::mir_codegen::compile_mir(&mut mir, "journal_fail_closed").expect("compile")
}

#[test]
fn persistent_compiled_message_is_not_executed_without_durable_admission() {
    let module = compile(
        r#"
        persistent actor Counter {
            state durable count: Int = 0
            behavior bump() { self.count = self.count + 1 }
        }
        "#,
    );
    let id = module.actor_metadata[0].behavior_indices[0];
    let mut runtime = Runtime::new();
    let actor_id = runtime
        .spawn_from_module(&module, id, vec![])
        .as_actor_id()
        .expect("spawn persistent Counter");
    runtime.persistence = Box::new(RejectJournalStore::default());

    runtime.send_message_by_id(actor_id, id as u16, &[]);
    runtime.run_scheduler();

    assert_eq!(
        runtime.actors[&actor_id]
            .get_state_field("count")
            .and_then(|value| value.as_int()),
        Some(0),
        "a denied durable journal admission must not mutate the actor"
    );
    assert!(runtime.persistence.read_journal(actor_id).is_empty());
}

fn native_bump(actor: &mut Actor, _args: &[Value]) {
    actor.set_state_field("count", Value::int(1));
}

#[test]
fn persistent_native_message_is_not_executed_without_durable_admission() {
    let mut runtime = Runtime::new();
    let actor_id = runtime.spawn_actor(Box::new(Vec::new));
    {
        let actor = runtime.actors.get_mut(&actor_id).unwrap();
        actor.persistent = true;
        actor.register_behavior("bump", native_bump);
        actor.set_state_field("count", Value::int(0));
    }
    runtime.persistence = Box::new(RejectJournalStore::default());
    runtime.send_message_by_id(actor_id, 0, &[]);
    runtime.run_scheduler();

    assert_eq!(
        runtime.actors[&actor_id]
            .get_state_field("count")
            .and_then(|value| value.as_int()),
        Some(0)
    );
    assert!(runtime.persistence.read_journal(actor_id).is_empty());
}

#[test]
fn ordinary_transient_actor_can_execute_when_durable_journal_is_unavailable() {
    let module = compile(
        r#"
        actor Counter {
            state count: Int = 0
            behavior bump() { self.count = self.count + 1 }
        }
        "#,
    );
    let id = module.actor_metadata[0].behavior_indices[0];
    let mut runtime = Runtime::new();
    let actor_id = runtime
        .spawn_from_module(&module, id, vec![])
        .as_actor_id()
        .expect("spawn transient Counter");
    runtime.persistence = Box::new(RejectJournalStore::default());

    runtime.send_message_by_id(actor_id, id as u16, &[]);
    runtime.run_scheduler();

    assert_eq!(
        runtime.actors[&actor_id]
            .get_state_field("count")
            .and_then(|value| value.as_int()),
        Some(1),
        "transient execution should not depend on durable journal availability"
    );
    assert!(runtime.persistence.read_journal(actor_id).is_empty());
}
