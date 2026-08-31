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
//! Limitations (single-node, single-entity dev/staging feature):
//! - `durable` (non-event-sourced) fields are only known at snapshot
//!   granularity; their intermediate values between snapshots are not
//!   reconstructible without re-execution and are reported from the base
//!   snapshot (or the declared defaults when no snapshot precedes N).
//! - Cluster-wide, vector-clock rewind is out of scope.
//! - Gated on the durable store being enabled (`NULANG_STORE_PATH` or
//!   `.nulang/store/`); with the default in-memory store there is nothing
//!   to rewind.

use std::collections::BTreeMap;
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
}
