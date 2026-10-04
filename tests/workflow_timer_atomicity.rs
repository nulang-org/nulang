use nulang::runtime::{
    ActorSnapshot, DurableCommit, DurableTransition, EventEntry, JournalEntry, MemoryStore,
    PersistenceStore, Runtime, WorkflowEvent,
};
use nulang::vm::Value;
use std::collections::HashMap;
use std::io;

/// Fault-injection store that models the exact legacy partial-commit window:
/// workflow-event appends succeed, but snapshot persistence fails. Atomic
/// transitions fail before mutating the inner store.
struct AppendThenFailSnapshotStore {
    inner: MemoryStore,
}

impl AppendThenFailSnapshotStore {
    fn new() -> Self {
        Self {
            inner: MemoryStore::new(),
        }
    }
}

impl PersistenceStore for AppendThenFailSnapshotStore {
    fn commit_transition(&mut self, _transition: DurableTransition) -> io::Result<DurableCommit> {
        Err(io::Error::new(
            io::ErrorKind::Other,
            "injected atomic transition failure",
        ))
    }

    fn save_snapshot(&mut self, _snapshot: ActorSnapshot) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Other,
            "injected snapshot failure",
        ))
    }

    fn load_snapshot(&self, actor_id: u64) -> Option<ActorSnapshot> {
        self.inner.load_snapshot(actor_id)
    }

    fn append_journal(&mut self, actor_id: u64, entry: JournalEntry) -> io::Result<()> {
        self.inner.append_journal(actor_id, entry)
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

    fn append_event(&mut self, actor_id: u64, entry: EventEntry) -> io::Result<()> {
        self.inner.append_event(actor_id, entry)
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

#[test]
fn failed_timer_transition_leaves_no_durable_timer_record_or_live_timer() {
    let mut runtime = Runtime::new();
    let actor_id = runtime.spawn_workflow_actor(
        "AtomicTimerController",
        Box::new(|| vec![("step_index".to_string(), Value::int(0))]),
        HashMap::new(),
    );
    runtime.persistence = Box::new(AppendThenFailSnapshotStore::new());

    let result = runtime.schedule_workflow_timer(actor_id, "__reconcile_retry:g1:r1", 100);

    assert!(result.is_err());
    assert!(runtime.timer_wheel.is_empty());
    assert!(
        runtime.persistence.read_timer_events(actor_id).is_empty(),
        "a failed timer transition must not leave a durable TimerSet behind"
    );
}
