//! Generated atomic-transition invariants over fresh MemoryStore instances.
//! This is a store-contract test, not a process-kill/disk-reopen durability test.

use nulang::dst::property::{check_trace_property, TracePropertyConfig};
use nulang::dst::DeterministicRng;
use nulang::runtime::{
    ActorSnapshot, DurableTail, DurableTransition, EventEntry, JournalEntry, MemoryStore,
    PersistedValue, PersistenceStore, DURABLE_TRANSITION_VERSION,
};
use std::collections::HashMap;
use std::io::ErrorKind;

const ACTOR: u64 = 73021;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    Commit(i64),
    AdvanceEpoch(i64),
    ExactRetry,
    ConflictingRetry,
    StaleEpoch,
    WrongPredecessor,
}

fn generated_steps(rng: &mut DeterministicRng) -> Vec<Step> {
    let len = 1 + rng.next() % 40;
    (0..len)
        .map(|_| {
            let value = (rng.next() % 200) as i64;
            match rng.next() % 6 {
                0 => Step::Commit(value),
                1 => Step::AdvanceEpoch(value),
                2 => Step::ExactRetry,
                3 => Step::ConflictingRetry,
                4 => Step::StaleEpoch,
                _ => Step::WrongPredecessor,
            }
        })
        .collect()
}

fn transition(epoch: u64, previous: u64, value: i64) -> DurableTransition {
    let sequence = previous + 1;
    let mut state = HashMap::new();
    state.insert("value".to_string(), PersistedValue::Int(value));

    DurableTransition {
        version: DURABLE_TRANSITION_VERSION,
        actor_id: ACTOR,
        activation_epoch: epoch,
        sequence,
        expected_previous_sequence: previous,
        command: Some(JournalEntry {
            sequence,
            behavior_id: 1,
            payload: vec![PersistedValue::Int(value)],
        }),
        snapshot: Some(ActorSnapshot {
            actor_id: ACTOR,
            sequence,
            activation_epoch: epoch,
            state,
            ..ActorSnapshot::default()
        }),
        workflow_events: vec![],
        domain_events: vec![EventEntry {
            sequence,
            field_name: "value".to_string(),
            event_name: "Assigned".to_string(),
            args: vec![],
            value: PersistedValue::Int(value),
        }],
        durable_effects: vec![],
        outbox: vec![],
    }
}

type SnapshotState = Option<(u64, u64, Option<PersistedValue>)>;
type StoreWitness = (Option<DurableTail>, SnapshotState, u64, usize, usize, usize);

fn witness(store: &MemoryStore) -> StoreWitness {
    let snapshot = store.load_snapshot(ACTOR).map(|snapshot| {
        (
            snapshot.sequence,
            snapshot.activation_epoch,
            snapshot.state.get("value").cloned(),
        )
    });
    (
        store.durable_tail(ACTOR),
        snapshot,
        store.latest_sequence(ACTOR),
        store.committed_transitions(ACTOR).len(),
        store.read_journal(ACTOR).len(),
        store.read_events(ACTOR).len(),
    )
}

fn transition_contract_holds(script: &[Step]) -> bool {
    // A new store per predicate invocation is essential for deterministic
    // shrinking; no mutable state is shared between candidate replays.
    let mut store = MemoryStore::new();
    let mut epoch = 1u64;
    let mut accepted = 0usize;
    let mut last_accepted: Option<DurableTransition> = None;

    for &operation in script {
        let before = witness(&store);
        let sequence = before.2;

        match operation {
            Step::Commit(value) | Step::AdvanceEpoch(value) => {
                let proposed_epoch = if matches!(operation, Step::AdvanceEpoch(_)) {
                    epoch + 1
                } else {
                    epoch
                };
                let tx = transition(proposed_epoch, sequence, value);
                let Ok(commit) = store.commit_transition(tx.clone()) else {
                    return false;
                };
                if commit.actor_id != ACTOR
                    || commit.activation_epoch != proposed_epoch
                    || commit.sequence != sequence + 1
                    || commit.digest != tx.digest().unwrap()
                {
                    return false;
                }
                accepted += 1;
                epoch = proposed_epoch;
                last_accepted = Some(tx);
                if store.latest_sequence(ACTOR) != accepted as u64
                    || store.committed_transitions(ACTOR).len() != accepted
                    || store.read_journal(ACTOR).len() != accepted
                    || store.read_events(ACTOR).len() != accepted
                    || store.load_snapshot(ACTOR).map(|s| s.state.get("value").cloned())
                        != Some(Some(PersistedValue::Int(value)))
                {
                    return false;
                }
            }
            Step::ExactRetry => {
                if let Some(tx) = &last_accepted {
                    let Ok(commit) = store.commit_transition(tx.clone()) else {
                        return false;
                    };
                    if commit.sequence != tx.sequence || commit.digest != tx.digest().unwrap() {
                        return false;
                    }
                    if witness(&store) != before {
                        return false;
                    }
                }
            }
            Step::ConflictingRetry => {
                if let Some(tx) = &last_accepted {
                    let mut conflict = tx.clone();
                    let snapshot = conflict.snapshot.as_mut().unwrap();
                    let PersistedValue::Int(original) = snapshot.state["value"] else {
                        return false;
                    };
                    snapshot.state.insert(
                        "value".to_string(),
                        PersistedValue::Int(original + 1),
                    );
                    if !matches!(
                        store.commit_transition(conflict),
                        Err(ref err) if err.kind() == ErrorKind::AlreadyExists
                    ) || witness(&store) != before
                    {
                        return false;
                    }
                }
            }
            Step::StaleEpoch => {
                if epoch > 1 {
                    let stale = transition(epoch - 1, sequence, 21);
                    if !matches!(
                        store.commit_transition(stale),
                        Err(ref err) if err.kind() == ErrorKind::PermissionDenied
                    ) || witness(&store) != before
                    {
                        return false;
                    }
                }
            }
            Step::WrongPredecessor => {
                // Structurally valid, but not based on the actually committed
                // sequence. Must not partially append journal/events/snapshot.
                let gap = transition(epoch, sequence + 1, 22);
                if !matches!(
                    store.commit_transition(gap),
                    Err(ref err) if err.kind() == ErrorKind::InvalidData
                ) || witness(&store) != before
                {
                    return false;
                }
            }
        }
    }

    witness(&store).2 == accepted as u64
}

#[test]
fn generated_memory_store_commit_retry_and_fencing_laws() {
    let result = check_trace_property(
        TracePropertyConfig {
            seed: 0xd0a8,
            cases: 96,
            max_shrink_replays: 300,
        },
        generated_steps,
        transition_contract_holds,
    );
    assert_eq!(result, Ok(96), "durable transition property: {result:?}");
}

#[test]
fn explicit_conflict_and_stale_owner_do_not_publish_partial_state() {
    let script = [
        Step::Commit(12),
        Step::ExactRetry,
        Step::ConflictingRetry,
        Step::AdvanceEpoch(13),
        Step::StaleEpoch,
        Step::WrongPredecessor,
        Step::ExactRetry,
    ];
    assert!(transition_contract_holds(&script));
}
