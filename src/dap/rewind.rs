//! Time-travel rewind for durable entities (Wave E3).
//!
//! Rewind reconstructs the state of a durable actor/entity *as of message
//! #N* from the persistence store, without re-executing behavior bytecode:
//!
//! 1. The latest snapshot whose `sequence <= N` is the base state.
//! 2. Event-sourcing events (`events.jsonl`) with `sequence <= N` are
//!    overlaid in order, using the **recorded post-apply value** stored in
//!    each [`EventEntry::value`] — replay is therefore deterministic by
//!    construction: no user code runs during rewind, so no non-deterministic
//!    effect can leak into the reconstructed state. (Per SPEC2 §9.7 /
//!    §9.7a, behavior bodies that perform non-deterministic effects must
//!    source them from journaled effects; the event log's recorded values
//!    are exactly that journal for `event_sourced` fields.)
//! 3. Journal entries (`journal.jsonl`) with `sequence <= N` are listed so
//!    the client can see *which* message the entity is rewound to.
//!
//! Stepping forward from a rewound position replays the next recorded
//! events (`N -> N+1`) — again from recorded values, never by re-execution.
//!
//! A rewind can also be captured as a [`DurableBranch`]. A branch is an
//! immutable counterfactual view with explicit lineage metadata. It does not
//! mutate the live entity or allocate a new runtime actor yet; that separation
//! keeps debugger branching deterministic while the runtime-level branch
//! activation contract (identity, code version, CRDT/workflow state) evolves.
//!
//! Limitations (single-node, single-entity dev/staging feature):
//! - `durable` (non-event-sourced) fields are only known at snapshot
//!   granularity; their intermediate values between snapshots are not
//!   reconstructible without re-execution and are reported from the base
//!   snapshot (or the declared defaults when no snapshot precedes N).
//! - Cluster-wide, vector-clock rewind is out of scope.
//! - Gated on the durable store being enabled (`NULANG_STORE_PATH` or
//!   `.nulang/store/`); with the default in-memory store there is nothing
//!   to rewind.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use crate::runtime::{EventEntry, JournalEntry, JsonFileStore, PersistedValue, PersistenceStore};

/// State of one durable entity reconstructed as of `sequence`.
#[derive(Debug, Clone)]
pub struct RewoundState {
    pub actor_id: u64,
    /// The sequence number this state represents (the rewind target,
    /// clamped to `latest_sequence`).
    pub sequence: u64,
    /// Highest sequence number recorded for the entity.
    pub latest_sequence: u64,
    /// Sequence of the snapshot used as the replay base (0 = none).
    pub snapshot_sequence: u64,
    /// Reconstructed durable/event-sourced field values.
    pub state: BTreeMap<String, PersistedValue>,
    /// Journaled messages (events) with `sequence <= self.sequence`,
    /// in delivery order.
    pub journal: Vec<JournalEntry>,
    /// Event-sourcing events with `sequence <= self.sequence`.
    pub events: Vec<EventEntry>,
}

/// Immutable counterfactual branch captured from an entity's durable history.
///
/// This is deliberately a *view*, not a newly activated actor. Runtime-level
/// activation needs stronger guarantees than debugger rewind currently has:
/// stable code-version identity, workflow suspension state, CRDT lineage and
/// cluster-wide ownership must all be explicit before a branch may execute.
/// Keeping the first primitive immutable makes it safe to use for inspection,
/// state comparison, speculative planning and future shadow-replay tooling.
#[derive(Debug, Clone)]
pub struct DurableBranch {
    /// Caller-defined stable branch identifier (for example a UUID or slug).
    pub branch_id: String,
    /// Entity whose history this branch derives from.
    pub parent_actor_id: u64,
    /// Historical sequence at which the branch diverges.
    pub fork_sequence: u64,
    /// Parent head when the branch was captured.
    pub parent_latest_sequence: u64,
    /// Snapshot sequence used as the reconstruction base.
    pub snapshot_sequence: u64,
    /// Reconstructed field state at the fork point.
    pub state: BTreeMap<String, PersistedValue>,
    /// Parent message history through the fork point.
    pub journal: Vec<JournalEntry>,
    /// Parent event-sourcing history through the fork point.
    pub events: Vec<EventEntry>,
}

/// One field-level difference between two reconstructed durable states.
#[derive(Debug, Clone, PartialEq)]
pub struct StateDiff {
    pub field: String,
    pub before: Option<PersistedValue>,
    pub after: Option<PersistedValue>,
}

/// Reconstruct the state of `actor_id` as of message `target_seq`:
/// snapshot overlay + replay of events `1..=target_seq` (recorded values).
pub fn rewind_entity(store: &dyn PersistenceStore, actor_id: u64, target_seq: u64) -> RewoundState {
    let latest = store.latest_sequence(actor_id);
    let target = target_seq.min(latest);

    // Base: the latest snapshot taken at or before the target. A snapshot
    // taken *after* the target already contains post-target mutations, so
    // it cannot be used as a base (the entity's declared defaults apply).
    let snapshot = store
        .load_snapshot(actor_id)
        .filter(|s| s.sequence <= target);
    let snapshot_sequence = snapshot.as_ref().map(|s| s.sequence).unwrap_or(0);
    let mut state: BTreeMap<String, PersistedValue> = snapshot
        .map(|s| s.state.into_iter().collect())
        .unwrap_or_default();

    // Replay event-sourcing events up to the target, in log order. Each
    // entry carries its recorded post-apply value, so this overlay is a
    // pure function of the log — deterministic replay (SPEC2 §9.7).
    let events: Vec<EventEntry> = store
        .read_events(actor_id)
        .into_iter()
        .filter(|e| e.sequence <= target)
        .collect();
    for e in &events {
        state.insert(e.field_name.clone(), e.value.clone());
    }

    let journal: Vec<JournalEntry> = store
        .read_journal(actor_id)
        .into_iter()
        .filter(|j| j.sequence <= target)
        .collect();

    RewoundState {
        actor_id,
        sequence: target,
        latest_sequence: latest,
        snapshot_sequence,
        state,
        journal,
        events,
    }
}

/// Capture an immutable durable branch at `target_seq`.
///
/// The target follows rewind semantics and is clamped to the parent head.
/// No persistence is written and the live parent is never mutated. This API
/// intentionally separates deterministic branch *construction* from branch
/// *activation*, which will require code-version and distributed-ownership
/// contracts before it is safe in production.
pub fn fork_entity_view(
    store: &dyn PersistenceStore,
    actor_id: u64,
    target_seq: u64,
    branch_id: impl Into<String>,
) -> DurableBranch {
    let rewound = rewind_entity(store, actor_id, target_seq);
    DurableBranch {
        branch_id: branch_id.into(),
        parent_actor_id: rewound.actor_id,
        fork_sequence: rewound.sequence,
        parent_latest_sequence: rewound.latest_sequence,
        snapshot_sequence: rewound.snapshot_sequence,
        state: rewound.state,
        journal: rewound.journal,
        events: rewound.events,
    }
}

/// Compute a deterministic field-level diff between two reconstructed states.
///
/// The result is sorted by field name because both inputs are `BTreeMap`s and
/// the union is collected into a `BTreeSet`. Equal fields are omitted.
pub fn diff_states(
    before: &BTreeMap<String, PersistedValue>,
    after: &BTreeMap<String, PersistedValue>,
) -> Vec<StateDiff> {
    let keys: BTreeSet<&String> = before.keys().chain(after.keys()).collect();
    keys.into_iter()
        .filter_map(|field| {
            let old = before.get(field);
            let new = after.get(field);
            if old == new {
                None
            } else {
                Some(StateDiff {
                    field: field.clone(),
                    before: old.cloned(),
                    after: new.cloned(),
                })
            }
        })
        .collect()
}

/// Compare a branch point with the parent's current durable head.
///
/// This is useful for debugger UIs and shadow-migration tooling: it answers
/// "what changed on the real timeline after this branch diverged?" without
/// executing user code.
pub fn diff_branch_from_parent_head(
    store: &dyn PersistenceStore,
    branch: &DurableBranch,
) -> Vec<StateDiff> {
    let parent_head = rewind_entity(store, branch.parent_actor_id, u64::MAX);
    diff_states(&branch.state, &parent_head.state)
}

/// Step forward one message from a rewound position: replays the recorded
/// events for `state.sequence + 1` (no-op when already at the latest).
pub fn step_forward(store: &dyn PersistenceStore, state: &RewoundState) -> RewoundState {
    rewind_entity(store, state.actor_id, state.sequence.saturating_add(1))
}

/// Resolve the durable store directory the same way the CLI does
/// (`NULANG_STORE_PATH` env var, else `.nulang/store/`), returning `None`
/// when the store is not enabled (rewind is gated on this).
pub fn resolve_store_dir() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("NULANG_STORE_PATH") {
        if !p.is_empty() {
            return Some(PathBuf::from(p));
        }
    }
    let default = PathBuf::from(".nulang/store");
    if default.is_dir() {
        Some(default)
    } else {
        None
    }
}

/// Open the durable store for rewind, or `None` when it is not enabled.
pub fn open_store() -> Option<JsonFileStore> {
    resolve_store_dir().and_then(|dir| JsonFileStore::new(dir).ok())
}

/// Open the store at an explicit path (used by tests, and by the server
/// when constructed with a store override).
pub fn open_store_at(dir: &std::path::Path) -> Option<JsonFileStore> {
    JsonFileStore::new(dir).ok()
}

fn persisted_value_json(v: &PersistedValue) -> serde_json::Value {
    match v {
        PersistedValue::Int(i) => serde_json::json!(*i),
        PersistedValue::Float(f) => serde_json::json!(*f),
        PersistedValue::Bool(b) => serde_json::json!(*b),
        PersistedValue::String(s) => serde_json::json!(s),
        PersistedValue::Nil => serde_json::Value::Null,
        PersistedValue::Unit => serde_json::json!("unit"),
        PersistedValue::Actor(a) => serde_json::json!(format!("<actor {}>", a)),
    }
}

impl RewoundState {
    /// DAP-facing JSON view of the rewound state.
    pub fn to_json(&self) -> serde_json::Value {
        let state: serde_json::Map<String, serde_json::Value> = self
            .state
            .iter()
            .map(|(k, v)| (k.clone(), persisted_value_json(v)))
            .collect();
        let journal: Vec<serde_json::Value> = self
            .journal
            .iter()
            .map(|j| {
                serde_json::json!({
                    "sequence": j.sequence,
                    "behaviorId": j.behavior_id,
                    "payload": j.payload.iter().map(persisted_value_json).collect::<Vec<_>>(),
                })
            })
            .collect();
        serde_json::json!({
            "actorId": self.actor_id,
            "sequence": self.sequence,
            "latestSequence": self.latest_sequence,
            "snapshotSequence": self.snapshot_sequence,
            "state": state,
            "journal": journal,
        })
    }
}

impl DurableBranch {
    /// JSON view intended for debugger and Cloud timeline clients.
    pub fn to_json(&self) -> serde_json::Value {
        let state: serde_json::Map<String, serde_json::Value> = self
            .state
            .iter()
            .map(|(k, v)| (k.clone(), persisted_value_json(v)))
            .collect();
        serde_json::json!({
            "branchId": self.branch_id,
            "parentActorId": self.parent_actor_id,
            "forkSequence": self.fork_sequence,
            "parentLatestSequence": self.parent_latest_sequence,
            "snapshotSequence": self.snapshot_sequence,
            "state": state,
            "journalLength": self.journal.len(),
            "eventLength": self.events.len(),
            "executable": false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::MemoryStore;

    /// Build a store with a snapshot at seq 2 (counter=10) and events
    /// 3..=5 incrementing `count` (recorded values 11, 12, 13).
    fn sample_store() -> MemoryStore {
        let mut store = MemoryStore::new();
        let mut snap = crate::runtime::ActorSnapshot::default();
        snap.actor_id = 7;
        snap.sequence = 2;
        snap.state
            .insert("count".to_string(), PersistedValue::Int(10));
        snap.state
            .insert("name".to_string(), PersistedValue::String("c".to_string()));
        store.save_snapshot(snap).unwrap();
        for (seq, val) in [(3u64, 11i64), (4, 12), (5, 13)] {
            store
                .append_event(
                    7,
                    EventEntry {
                        sequence: seq,
                        field_name: "count".to_string(),
                        event_name: "Incremented".to_string(),
                        args: vec![PersistedValue::Int(1)],
                        value: PersistedValue::Int(val),
                    },
                )
                .unwrap();
            store
                .append_journal(
                    7,
                    JournalEntry {
                        sequence: seq,
                        behavior_id: 0,
                        payload: vec![PersistedValue::Int(1)],
                    },
                )
                .unwrap();
        }
        store
    }

    #[test]
    fn rewind_to_latest_uses_snapshot_plus_all_events() {
        let store = sample_store();
        let st = rewind_entity(&store, 7, 5);
        assert_eq!(st.sequence, 5);
        assert_eq!(st.snapshot_sequence, 2);
        assert_eq!(st.state.get("count"), Some(&PersistedValue::Int(13)));
        assert_eq!(
            st.state.get("name"),
            Some(&PersistedValue::String("c".to_string()))
        );
        assert_eq!(st.journal.len(), 3);
    }

    #[test]
    fn rewind_to_n_replays_events_up_to_n() {
        let store = sample_store();
        // Rewind to message #3: events 1..=3 replayed -> count = 11.
        let st = rewind_entity(&store, 7, 3);
        assert_eq!(st.sequence, 3);
        assert_eq!(st.state.get("count"), Some(&PersistedValue::Int(11)));
        assert_eq!(st.journal.len(), 1);
        assert_eq!(st.events.len(), 1);
    }

    #[test]
    fn rewind_to_snapshot_sequence_uses_snapshot_only() {
        let store = sample_store();
        let st = rewind_entity(&store, 7, 2);
        assert_eq!(st.state.get("count"), Some(&PersistedValue::Int(10)));
        assert!(st.events.is_empty());
        assert!(st.journal.is_empty());
    }

    #[test]
    fn rewind_beyond_latest_clamps() {
        let store = sample_store();
        let st = rewind_entity(&store, 7, 999);
        assert_eq!(st.sequence, 5);
        assert_eq!(st.latest_sequence, 5);
        assert_eq!(st.state.get("count"), Some(&PersistedValue::Int(13)));
    }

    #[test]
    fn forward_step_after_rewind_replays_next_event() {
        let store = sample_store();
        let st = rewind_entity(&store, 7, 3);
        let fwd = step_forward(&store, &st);
        assert_eq!(fwd.sequence, 4);
        assert_eq!(fwd.state.get("count"), Some(&PersistedValue::Int(12)));
        let fwd2 = step_forward(&store, &fwd);
        assert_eq!(fwd2.state.get("count"), Some(&PersistedValue::Int(13)));
        // Stepping at the head is a no-op.
        let fwd3 = step_forward(&store, &fwd2);
        assert_eq!(fwd3.sequence, 5);
        assert_eq!(fwd3.state.get("count"), Some(&PersistedValue::Int(13)));
    }

    #[test]
    fn rewind_without_snapshot_replays_from_defaults() {
        let mut store = MemoryStore::new();
        store
            .append_event(
                9,
                EventEntry {
                    sequence: 1,
                    field_name: "seen".to_string(),
                    event_name: "Custom".to_string(),
                    args: vec![],
                    value: PersistedValue::Bool(true),
                },
            )
            .unwrap();
        let st = rewind_entity(&store, 9, 1);
        assert_eq!(st.snapshot_sequence, 0);
        assert_eq!(st.state.get("seen"), Some(&PersistedValue::Bool(true)));
    }

    #[test]
    fn fork_view_captures_lineage_without_mutating_parent() {
        let store = sample_store();
        let branch = fork_entity_view(&store, 7, 3, "candidate-a");

        assert_eq!(branch.branch_id, "candidate-a");
        assert_eq!(branch.parent_actor_id, 7);
        assert_eq!(branch.fork_sequence, 3);
        assert_eq!(branch.parent_latest_sequence, 5);
        assert_eq!(branch.snapshot_sequence, 2);
        assert_eq!(branch.state.get("count"), Some(&PersistedValue::Int(11)));
        assert_eq!(branch.journal.len(), 1);
        assert_eq!(branch.events.len(), 1);

        // Capturing a branch is read-only: the parent remains at its head.
        let parent = rewind_entity(&store, 7, u64::MAX);
        assert_eq!(parent.sequence, 5);
        assert_eq!(parent.state.get("count"), Some(&PersistedValue::Int(13)));
    }

    #[test]
    fn branch_diff_reports_parent_changes_after_divergence() {
        let store = sample_store();
        let branch = fork_entity_view(&store, 7, 3, "candidate-a");
        let diff = diff_branch_from_parent_head(&store, &branch);

        assert_eq!(
            diff,
            vec![StateDiff {
                field: "count".to_string(),
                before: Some(PersistedValue::Int(11)),
                after: Some(PersistedValue::Int(13)),
            }]
        );
    }

    #[test]
    fn diff_states_reports_added_removed_and_changed_fields_in_order() {
        let before = BTreeMap::from([
            ("changed".to_string(), PersistedValue::Int(1)),
            ("removed".to_string(), PersistedValue::Bool(true)),
            ("same".to_string(), PersistedValue::String("x".to_string())),
        ]);
        let after = BTreeMap::from([
            ("added".to_string(), PersistedValue::Int(9)),
            ("changed".to_string(), PersistedValue::Int(2)),
            ("same".to_string(), PersistedValue::String("x".to_string())),
        ]);

        let diff = diff_states(&before, &after);
        assert_eq!(
            diff,
            vec![
                StateDiff {
                    field: "added".to_string(),
                    before: None,
                    after: Some(PersistedValue::Int(9)),
                },
                StateDiff {
                    field: "changed".to_string(),
                    before: Some(PersistedValue::Int(1)),
                    after: Some(PersistedValue::Int(2)),
                },
                StateDiff {
                    field: "removed".to_string(),
                    before: Some(PersistedValue::Bool(true)),
                    after: None,
                },
            ]
        );
    }
}