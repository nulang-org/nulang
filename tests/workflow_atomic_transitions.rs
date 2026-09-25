use std::io;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use nulang::runtime::{
    ActorSnapshot, DurableCommit, DurableTransition, EventEntry, JournalEntry, MemoryStore,
    PersistedValue, PersistenceStore, Runtime, StateModel, WorkflowEvent,
};
use nulang::vm::Value;

struct CountingStore {
    inner: MemoryStore,
    atomic_commits: Arc<AtomicUsize>,
    legacy_journal_writes: Arc<AtomicUsize>,
    legacy_workflow_writes: Arc<AtomicUsize>,
    legacy_snapshot_writes: Arc<AtomicUsize>,
    fail_at_atomic_commit: Option<usize>,
}

impl CountingStore {
    fn new(
        atomic_commits: Arc<AtomicUsize>,
        legacy_journal_writes: Arc<AtomicUsize>,
        legacy_workflow_writes: Arc<AtomicUsize>,
        legacy_snapshot_writes: Arc<AtomicUsize>,
    ) -> Self {
        Self {
            inner: MemoryStore::new(),
            atomic_commits,
            legacy_journal_writes,
            legacy_workflow_writes,
            legacy_snapshot_writes,
            fail_at_atomic_commit: None,
        }
    }

    fn fail_at_atomic_commit(mut self, attempt: usize) -> Self {
        self.fail_at_atomic_commit = Some(attempt);
        self
    }
}

impl PersistenceStore for CountingStore {
    fn commit_transition(&mut self, transition: DurableTransition) -> io::Result<DurableCommit> {
        let attempt = self.atomic_commits.fetch_add(1, Ordering::SeqCst) + 1;
        if self.fail_at_atomic_commit == Some(attempt) {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "injected durable transition failure",
            ));
        }
        self.inner.commit_transition(transition)
    }

    fn save_snapshot(&mut self, snapshot: ActorSnapshot) -> io::Result<()> {
        self.legacy_snapshot_writes.fetch_add(1, Ordering::SeqCst);
        self.inner.save_snapshot(snapshot)
    }

    fn load_snapshot(&self, actor_id: u64) -> Option<ActorSnapshot> {
        self.inner.load_snapshot(actor_id)
    }

    fn append_journal(&mut self, actor_id: u64, entry: JournalEntry) -> io::Result<()> {
        self.legacy_journal_writes.fetch_add(1, Ordering::SeqCst);
        self.inner.append_journal(actor_id, entry)
    }

    fn read_journal(&self, actor_id: u64) -> Vec<JournalEntry> {
        self.inner.read_journal(actor_id)
    }

    fn append_workflow_event(&mut self, actor_id: u64, event: WorkflowEvent) -> io::Result<()> {
        self.legacy_workflow_writes.fetch_add(1, Ordering::SeqCst);
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
fn workflow_start_and_completion_use_atomic_transition_boundary() {
    let atomic_commits = Arc::new(AtomicUsize::new(0));
    let legacy_journal_writes = Arc::new(AtomicUsize::new(0));
    let legacy_workflow_writes = Arc::new(AtomicUsize::new(0));
    let legacy_snapshot_writes = Arc::new(AtomicUsize::new(0));

    let mut runtime = Runtime::new();
    runtime.persistence = Box::new(CountingStore::new(
        atomic_commits.clone(),
        legacy_journal_writes.clone(),
        legacy_workflow_writes.clone(),
        legacy_snapshot_writes.clone(),
    ));

    let mut models = std::collections::HashMap::new();
    models.insert("step_index".to_string(), StateModel::Durable);
    let actor_id = runtime.spawn_workflow_actor(
        "AtomicWorkflow",
        Box::new(|| vec![("step_index".to_string(), Value::int(0))]),
        models,
    );
    assert_ne!(actor_id, 0);
    assert_eq!(atomic_commits.load(Ordering::SeqCst), 1);
    assert_eq!(legacy_journal_writes.load(Ordering::SeqCst), 0);
    assert_eq!(legacy_workflow_writes.load(Ordering::SeqCst), 0);
    assert_eq!(legacy_snapshot_writes.load(Ordering::SeqCst), 0);

    runtime
        .actors
        .get_mut(&actor_id)
        .unwrap()
        .register_behavior("next", |actor, _args| {
            let current = actor
                .get_state_field("step_index")
                .and_then(|value| value.as_int())
                .unwrap_or(0);
            actor.set_state_field("step_index", Value::int(current + 1));
        });

    runtime.send_message(actor_id, "next", &[]);
    runtime.run_scheduler();

    assert_eq!(atomic_commits.load(Ordering::SeqCst), 2);
    assert_eq!(legacy_journal_writes.load(Ordering::SeqCst), 0);
    assert_eq!(legacy_workflow_writes.load(Ordering::SeqCst), 0);
    assert_eq!(legacy_snapshot_writes.load(Ordering::SeqCst), 0);

    let events = runtime.persistence.read_workflow_events(actor_id);
    assert_eq!(events.len(), 2);
    assert!(matches!(
        events.as_slice(),
        [
            WorkflowEvent::WorkflowStarted { .. },
            WorkflowEvent::StepCompleted { .. }
        ]
    ));

    let journal = runtime.persistence.read_journal(actor_id);
    assert_eq!(journal.len(), 1);
    let completed_sequence = match &events[1] {
        WorkflowEvent::StepCompleted { sequence, .. } => *sequence,
        other => panic!("expected StepCompleted, got {other:?}"),
    };
    assert_eq!(journal[0].sequence, completed_sequence);

    let snapshot = runtime.persistence.load_snapshot(actor_id).unwrap();
    assert_eq!(snapshot.sequence, completed_sequence);
    assert_eq!(
        snapshot.state.get("step_index"),
        Some(&PersistedValue::Int(1))
    );
}

#[test]
fn atomic_transition_can_follow_transitional_legacy_journal_sequence() {
    let mut store = MemoryStore::new();

    let first = DurableTransition {
        version: nulang::runtime::DURABLE_TRANSITION_VERSION,
        actor_id: 7,
        activation_epoch: 1,
        sequence: 1,
        expected_previous_sequence: 0,
        command: None,
        snapshot: None,
        workflow_events: vec![WorkflowEvent::Custom {
            sequence: 1,
            name: "started".to_string(),
            args: Vec::new(),
        }],
        domain_events: Vec::new(),
        durable_effects: Vec::new(),
        outbox: Vec::new(),
    };
    store.commit_transition(first).unwrap();

    store
        .append_journal(
            7,
            JournalEntry {
                sequence: 2,
                behavior_id: 3,
                payload: Vec::new(),
            },
        )
        .unwrap();

    let next = DurableTransition {
        version: nulang::runtime::DURABLE_TRANSITION_VERSION,
        actor_id: 7,
        activation_epoch: 1,
        sequence: 3,
        expected_previous_sequence: 2,
        command: None,
        snapshot: None,
        workflow_events: vec![WorkflowEvent::Custom {
            sequence: 3,
            name: "completed".to_string(),
            args: Vec::new(),
        }],
        domain_events: Vec::new(),
        durable_effects: Vec::new(),
        outbox: Vec::new(),
    };

    store.commit_transition(next).unwrap();
    assert_eq!(store.latest_sequence(7), 3);
}

#[test]
fn workflow_turn_reuses_persisted_activation_epoch_without_runtime_cache() {
    let mut runtime = Runtime::new();
    let mut models = std::collections::HashMap::new();
    models.insert("step_index".to_string(), StateModel::Durable);

    let actor_id = runtime.spawn_workflow_actor(
        "EpochRecoveryWorkflow",
        Box::new(|| vec![("step_index".to_string(), Value::int(0))]),
        models,
    );
    assert_ne!(actor_id, 0);

    let mut snapshot = runtime.persistence.load_snapshot(actor_id).unwrap();
    snapshot.sequence = 2;
    runtime
        .persistence
        .commit_transition(DurableTransition {
            version: nulang::runtime::DURABLE_TRANSITION_VERSION,
            actor_id,
            activation_epoch: 7,
            sequence: 2,
            expected_previous_sequence: 1,
            command: None,
            snapshot: Some(snapshot),
            workflow_events: Vec::new(),
            domain_events: Vec::new(),
            durable_effects: Vec::new(),
            outbox: Vec::new(),
        })
        .unwrap();

    let tail = runtime
        .persistence
        .load_durable_tail(actor_id)
        .unwrap()
        .expect("atomic store should expose its committed tail");
    assert_eq!(tail.activation_epoch, 7);
    assert_eq!(tail.sequence, 2);

    runtime
        .append_signal_received(actor_id, "resume", Some("go".to_string()))
        .unwrap();

    let tail = runtime
        .persistence
        .load_durable_tail(actor_id)
        .unwrap()
        .expect("signal transition should preserve the committed epoch");
    assert_eq!(tail.activation_epoch, 7);
    assert_eq!(tail.sequence, 3);
}

#[test]
fn failed_workflow_commit_quarantines_actor_and_blocks_further_durable_input() {
    let atomic_commits = Arc::new(AtomicUsize::new(0));
    let legacy_journal_writes = Arc::new(AtomicUsize::new(0));
    let legacy_workflow_writes = Arc::new(AtomicUsize::new(0));
    let legacy_snapshot_writes = Arc::new(AtomicUsize::new(0));

    let store = CountingStore::new(
        atomic_commits.clone(),
        legacy_journal_writes.clone(),
        legacy_workflow_writes,
        legacy_snapshot_writes,
    )
    .fail_at_atomic_commit(2);

    let mut runtime = Runtime::new();
    runtime.persistence = Box::new(store);

    let mut models = std::collections::HashMap::new();
    models.insert("step_index".to_string(), StateModel::Durable);
    let actor_id = runtime.spawn_workflow_actor(
        "FailClosedWorkflow",
        Box::new(|| vec![("step_index".to_string(), Value::int(0))]),
        models,
    );
    assert_ne!(actor_id, 0);

    runtime
        .actors
        .get_mut(&actor_id)
        .unwrap()
        .register_behavior("next", |actor, _args| {
            actor.set_state_field("step_index", Value::int(1));
        });

    runtime.send_message(actor_id, "next", &[]);
    runtime.run_scheduler();

    assert_eq!(
        runtime.actors.get(&actor_id).unwrap().state,
        nulang::runtime::ActorState::Suspended
    );
    assert_eq!(atomic_commits.load(Ordering::SeqCst), 2);
    assert_eq!(legacy_journal_writes.load(Ordering::SeqCst), 0);
    assert!(runtime.persistence.read_journal(actor_id).is_empty());

    let committed_events = runtime.persistence.read_workflow_events(actor_id);
    assert_eq!(committed_events.len(), 1);
    assert!(matches!(
        &committed_events[0],
        WorkflowEvent::WorkflowStarted { .. }
    ));
    assert_eq!(
        runtime
            .persistence
            .load_snapshot(actor_id)
            .unwrap()
            .state
            .get("step_index"),
        Some(&PersistedValue::Int(0))
    );
    assert_eq!(
        runtime
            .actors
            .get(&actor_id)
            .and_then(|actor| actor.get_state_field("step_index"))
            .and_then(|value| value.as_int()),
        Some(0),
        "quarantine must roll live durable state back to the committed snapshot"
    );

    runtime.signal_workflow(actor_id, "resume", Some("ignored".to_string()));

    // The quarantine guard rejects the signal before touching the store or
    // actor signal queue, so dirty in-memory state cannot become durable later.
    assert_eq!(atomic_commits.load(Ordering::SeqCst), 2);
    assert!(runtime
        .actors
        .get(&actor_id)
        .unwrap()
        .received_signals
        .is_empty());
}

#[test]
fn recovery_restores_signal_committed_at_snapshot_sequence() {
    let mut runtime = Runtime::new();
    let mut models = std::collections::HashMap::new();
    models.insert("step_index".to_string(), StateModel::Durable);

    let actor_id = runtime.spawn_workflow_actor(
        "SignalRecoveryWorkflow",
        Box::new(|| vec![("step_index".to_string(), Value::int(0))]),
        models,
    );
    assert_ne!(actor_id, 0);

    runtime.signal_workflow(actor_id, "resume", Some("go".to_string()));
    assert_eq!(
        runtime.actors.get(&actor_id).unwrap().received_signals,
        vec![("resume".to_string(), Some("go".to_string()))]
    );

    runtime.actors.remove(&actor_id);
    assert_eq!(runtime.recover_actor(actor_id), Some(actor_id));

    assert_eq!(
        runtime.actors.get(&actor_id).unwrap().received_signals,
        vec![("resume".to_string(), Some("go".to_string()))]
    );
}

#[test]
fn recovery_restores_compensation_committed_at_snapshot_sequence() {
    let mut runtime = Runtime::new();
    let mut models = std::collections::HashMap::new();
    models.insert("step_index".to_string(), StateModel::Durable);

    let actor_id = runtime.spawn_workflow_actor(
        "CompensationRecoveryWorkflow",
        Box::new(|| vec![("step_index".to_string(), Value::int(0))]),
        models,
    );
    assert_ne!(actor_id, 0);

    runtime
        .append_saga_compensated(actor_id, "charge_card")
        .unwrap();

    runtime.actors.remove(&actor_id);
    assert_eq!(runtime.recover_actor(actor_id), Some(actor_id));

    assert_eq!(
        runtime.actors.get(&actor_id).unwrap().compensated_steps,
        vec!["charge_card".to_string()]
    );
}

#[test]
fn signal_while_suspended_preserves_last_committed_workflow_state() {
    let mut runtime = Runtime::new();
    let mut models = std::collections::HashMap::new();
    models.insert("step_index".to_string(), StateModel::Durable);

    let actor_id = runtime.spawn_workflow_actor(
        "SuspendedSignalWorkflow",
        Box::new(|| vec![("step_index".to_string(), Value::int(0))]),
        models,
    );
    assert_ne!(actor_id, 0);

    {
        let actor = runtime.actors.get_mut(&actor_id).unwrap();
        actor.set_state_field("step_index", Value::int(99));
        actor.waiting_signal = Some("resume".to_string());
    }

    runtime
        .append_signal_received(actor_id, "resume", Some("go".to_string()))
        .unwrap();

    let snapshot = runtime.persistence.load_snapshot(actor_id).unwrap();
    assert_eq!(snapshot.waiting_signal.as_deref(), Some("resume"));
    assert_eq!(
        snapshot.state.get("step_index"),
        Some(&PersistedValue::Int(0)),
        "signal commit must not snapshot partially executed workflow state"
    );

    let signals = runtime.persistence.read_signal_events(actor_id);
    assert_eq!(signals.len(), 1);
    assert_eq!(signals[0].sequence(), snapshot.sequence);
}
