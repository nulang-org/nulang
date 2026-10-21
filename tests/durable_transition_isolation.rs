//! PostgreSQL-style isolation schedules for Nulang durable transitions.
//! The schedule fixes the ordering of logical sessions without OS threads,
//! sleeps, nondeterministic timing, or mocks of the persistence contract.

use nulang::dst::isolation::{enumerate_interleavings, IsolationError, IsolationSchedule};
use nulang::runtime::{
    ActorSnapshot, DurableTransition, EventEntry, JournalEntry, MemoryStore, PersistedValue,
    PersistenceStore, DURABLE_TRANSITION_VERSION,
};
use std::collections::HashMap;

const ACTOR: u64 = 7_191;

fn proposal(store: &dyn PersistenceStore, lane: usize, epoch: u64) -> DurableTransition {
    let previous = store.latest_sequence(ACTOR);
    let sequence = previous + 1;
    let value = PersistedValue::Int(lane as i64 + 1);
    DurableTransition {
        version: DURABLE_TRANSITION_VERSION,
        actor_id: ACTOR,
        activation_epoch: epoch,
        sequence,
        expected_previous_sequence: previous,
        command: Some(JournalEntry {
            sequence,
            behavior_id: lane as u16 + 1,
            payload: vec![value.clone()],
        }),
        snapshot: Some(ActorSnapshot {
            actor_id: ACTOR,
            sequence,
            activation_epoch: epoch,
            state: HashMap::from([("owner".into(), value.clone())]),
            ..ActorSnapshot::default()
        }),
        workflow_events: vec![],
        domain_events: vec![EventEntry {
            sequence,
            field_name: "owner".into(),
            event_name: "Assigned".into(),
            args: vec![],
            value,
        }],
        durable_effects: vec![],
        outbox: vec![],
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Outcome {
    committed: Vec<(usize, u64)>,
    rejected: Vec<usize>,
}

/// Each lane performs Prepare -> Commit; the globally chosen order comes
/// from the isolation schedule, not thread or random scheduler timing.
fn run_schedule(store: &mut dyn PersistenceStore, schedule: &IsolationSchedule) -> Outcome {
    let mut prepared: [Option<DurableTransition>; 2] = [None, None];
    let mut outcome = Outcome {
        committed: vec![],
        rejected: vec![],
    };
    for step in &schedule.steps {
        match step.operation {
            0 => {
                prepared[step.lane] = Some(proposal(store, step.lane, 1));
            }
            1 => {
                let transition = prepared[step.lane]
                    .take()
                    .expect("commit must follow prepare in the same lane");
                match store.commit_transition(transition) {
                    Ok(receipt) => outcome.committed.push((step.lane, receipt.sequence)),
                    Err(_) => outcome.rejected.push(step.lane),
                }
            }
            _ => panic!("unknown step {}", step.operation),
        }
    }

    assert_eq!(
        store.read_journal(ACTOR).len(),
        outcome.committed.len(),
        "only committed transitions may append a command: {schedule:?}"
    );
    assert_eq!(
        store.read_events(ACTOR).len(),
        outcome.committed.len(),
        "domain events must commit atomically with commands: {schedule:?}"
    );
    assert_eq!(
        store.latest_sequence(ACTOR),
        outcome.committed.len() as u64,
        "rejected commits must not advance durable sequence: {schedule:?}"
    );
    let snapshot = store.load_snapshot(ACTOR).expect("at least one commit wins");
    let &(winner, tail_sequence) = outcome.committed.last().unwrap();
    assert_eq!(snapshot.sequence, tail_sequence);
    assert_eq!(snapshot.activation_epoch, 1);
    assert_eq!(
        snapshot.state.get("owner"),
        Some(&PersistedValue::Int(winner as i64 + 1))
    );
    let tail = store
        .load_durable_tail_position(ACTOR)
        .unwrap()
        .expect("atomic tail must exist");
    assert_eq!(tail.sequence, tail_sequence);
    assert_eq!(tail.activation_epoch, 1);

    outcome
}

#[test]
fn enumerates_all_permutations_while_preserving_each_session_order() {
    let schedules = enumerate_interleavings(&[2, 2], 6).unwrap();
    assert_eq!(schedules.len(), 6);
    for schedule in schedules {
        assert_eq!(schedule.steps.len(), 4);
        for lane in 0..2 {
            let operations: Vec<_> = schedule
                .steps
                .iter()
                .filter(|step| step.lane == lane)
                .map(|step| step.operation)
                .collect();
            assert_eq!(operations, vec![0, 1]);
        }
    }
}

#[test]
fn rejects_incomplete_exploration_instead_of_silently_truncating() {
    assert_eq!(
        enumerate_interleavings(&[2, 2], 5),
        Err(IsolationError::BudgetExceeded { limit: 5 })
    );
    assert_eq!(
        enumerate_interleavings(&[1], 0),
        Err(IsolationError::EmptyBudget)
    );
}

#[test]
fn competing_writers_preserve_atomicity_under_every_interleaving() {
    for schedule in enumerate_interleavings(&[2, 2], 6).unwrap() {
        let mut store = MemoryStore::new();
        let outcome = run_schedule(&mut store, &schedule);
        assert_eq!(outcome.committed.len() + outcome.rejected.len(), 2);
        assert!(outcome.committed.len() == 1 || outcome.committed.len() == 2);
        let sequences: Vec<_> = outcome.committed.iter().map(|(_, seq)| *seq).collect();
        let expected: Vec<_> = (1..=outcome.committed.len() as u64).collect();
        assert_eq!(sequences, expected, "commit order must be gapless: {schedule:?}");
    }
}

#[test]
fn exact_commit_retry_is_idempotent_and_conflicting_retry_is_not() {
    let mut store = MemoryStore::new();
    let first = proposal(&store, 0, 1);
    let initial = store.commit_transition(first.clone()).unwrap();
    assert_eq!(store.commit_transition(first).unwrap(), initial);
    assert_eq!(store.read_journal(ACTOR).len(), 1);
    assert_eq!(store.read_events(ACTOR).len(), 1);

    let conflicting = DurableTransition {
        command: Some(JournalEntry {
            sequence: 1,
            behavior_id: 9,
            payload: vec![],
        }),
        ..proposal_at_sequence(1, 0, 1)
    };
    let error = store.commit_transition(conflicting).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
    assert_eq!(store.latest_sequence(ACTOR), 1);
}

fn proposal_at_sequence(sequence: u64, lane: usize, epoch: u64) -> DurableTransition {
    let store = MemoryStore::new();
    let mut transition = proposal(&store, lane, epoch);
    transition.sequence = sequence;
    transition.expected_previous_sequence = sequence - 1;
    transition.command.as_mut().unwrap().sequence = sequence;
    transition.snapshot.as_mut().unwrap().sequence = sequence;
    transition.domain_events[0].sequence = sequence;
    transition
}

#[test]
fn new_activation_fences_stale_writer_without_partial_commits() {
    let mut store = MemoryStore::new();
    let initial = proposal(&store, 0, 1);
    store.commit_transition(initial).unwrap();
    let stale = proposal(&store, 0, 1);
    let promoted = proposal(&store, 1, 2);

    store.commit_transition(promoted).unwrap();
    let error = store.commit_transition(stale).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    assert_eq!(store.latest_sequence(ACTOR), 2);
    assert_eq!(store.read_journal(ACTOR).len(), 2);
    assert_eq!(store.read_events(ACTOR).len(), 2);
    assert_eq!(store.load_snapshot(ACTOR).unwrap().activation_epoch, 2);
}

#[cfg(feature = "sqlite")]
#[test]
fn libsql_and_memory_agree_on_every_competing_writer_interleaving() {
    use nulang::runtime::LibsqlStore;

    for schedule in enumerate_interleavings(&[2, 2], 6).unwrap() {
        let memory = run_schedule(&mut MemoryStore::new(), &schedule);
        let mut sqlite = LibsqlStore::in_memory().unwrap();
        let persisted = run_schedule(&mut sqlite, &schedule);
        assert_eq!(persisted, memory, "backend disagreement: {schedule:?}");
    }
}
