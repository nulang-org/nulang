//! Unified durable change stream.
//!
//! This module presents the runtime's existing durable journals as one
//! deterministic, resumable stream. It is intentionally storage-agnostic:
//! consumers can build indexes, incremental views, analytics projections,
//! replication feeds, and external CDC without knowing which persistence
//! backend stores the underlying records.

use super::persistence::{EventEntry, JournalEntry, PersistenceStore, WorkflowEvent};

/// Logical source of a durable change.
///
/// The explicit discriminant order is part of cursor ordering: when two
/// records share the same actor sequence, message delivery is observed first,
/// followed by state events and then workflow events.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    serde::Serialize,
    serde::Deserialize,
)]
#[repr(u8)]
#[serde(rename_all = "snake_case")]
pub enum DurableChangeLane {
    Journal = 0,
    Event = 1,
    Workflow = 2,
}

/// Stable resume cursor within one actor's durable history.
///
/// `sequence` alone is insufficient because a delivered message, one or more
/// event-sourced mutations, and workflow events may legitimately share the
/// same actor sequence. `lane` and `ordinal` prevent consumers from skipping
/// same-sequence records when resuming.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    serde::Serialize,
    serde::Deserialize,
)]
pub struct DurableChangeCursor {
    pub sequence: u64,
    pub lane: DurableChangeLane,
    /// Zero-based position inside the source log. The append-only persistence
    /// contract makes this stable for the lifetime of that log.
    pub ordinal: u64,
}

/// A committed durable record exposed through the unified change stream.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", content = "record", rename_all = "snake_case")]
pub enum DurableChangeRecord {
    Journal(JournalEntry),
    Event(EventEntry),
    Workflow(WorkflowEvent),
}

/// One item in an actor's unified durable change stream.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DurableChange {
    pub actor_id: u64,
    pub cursor: DurableChangeCursor,
    pub record: DurableChangeRecord,
}

impl DurableChange {
    pub fn sequence(&self) -> u64 {
        self.cursor.sequence
    }
}

/// Read an actor's durable history as one deterministic stream.
///
/// The current implementation is an adapter over the three existing append-
/// only logs. That keeps the API independent of Memory/JSON/libSQL storage and
/// lets higher layers adopt the unified model before a future storage-native
/// global commit log lands.
///
/// `after` is exclusive. Pass the last observed cursor to resume without
/// duplicates or gaps.
pub fn read_durable_changes(
    store: &dyn PersistenceStore,
    actor_id: u64,
    after: Option<DurableChangeCursor>,
) -> Vec<DurableChange> {
    let mut changes = Vec::new();

    for (ordinal, entry) in store.read_journal(actor_id).into_iter().enumerate() {
        changes.push(DurableChange {
            actor_id,
            cursor: DurableChangeCursor {
                sequence: entry.sequence,
                lane: DurableChangeLane::Journal,
                ordinal: ordinal as u64,
            },
            record: DurableChangeRecord::Journal(entry),
        });
    }

    for (ordinal, entry) in store.read_events(actor_id).into_iter().enumerate() {
        changes.push(DurableChange {
            actor_id,
            cursor: DurableChangeCursor {
                sequence: entry.sequence,
                lane: DurableChangeLane::Event,
                ordinal: ordinal as u64,
            },
            record: DurableChangeRecord::Event(entry),
        });
    }

    for (ordinal, entry) in store
        .read_workflow_events(actor_id)
        .into_iter()
        .enumerate()
    {
        changes.push(DurableChange {
            actor_id,
            cursor: DurableChangeCursor {
                sequence: entry.sequence(),
                lane: DurableChangeLane::Workflow,
                ordinal: ordinal as u64,
            },
            record: DurableChangeRecord::Workflow(entry),
        });
    }

    changes.sort_by_key(|change| change.cursor);

    if let Some(cursor) = after {
        changes.retain(|change| change.cursor > cursor);
    }

    changes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{MemoryStore, PersistedValue};

    #[test]
    fn merges_all_durable_logs_in_deterministic_order() {
        let mut store = MemoryStore::new();

        store
            .append_journal(
                7,
                JournalEntry {
                    sequence: 2,
                    behavior_id: 11,
                    payload: vec![PersistedValue::Int(1)],
                },
            )
            .unwrap();
        store
            .append_event(
                7,
                EventEntry {
                    sequence: 1,
                    field_name: "balance".into(),
                    event_name: "Credited".into(),
                    args: vec![PersistedValue::Int(10)],
                    value: PersistedValue::Int(10),
                },
            )
            .unwrap();
        store
            .append_workflow_event(
                7,
                WorkflowEvent::StepCompleted {
                    sequence: 3,
                    step_name: "settle".into(),
                },
            )
            .unwrap();

        let changes = read_durable_changes(&store, 7, None);
        assert_eq!(changes.len(), 3);
        assert_eq!(
            changes.iter().map(DurableChange::sequence).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
    }

    #[test]
    fn cursor_does_not_drop_same_sequence_records() {
        let mut store = MemoryStore::new();

        store
            .append_journal(
                9,
                JournalEntry {
                    sequence: 5,
                    behavior_id: 1,
                    payload: vec![],
                },
            )
            .unwrap();
        store
            .append_event(
                9,
                EventEntry {
                    sequence: 5,
                    field_name: "count".into(),
                    event_name: "Incremented".into(),
                    args: vec![],
                    value: PersistedValue::Int(1),
                },
            )
            .unwrap();
        store
            .append_workflow_event(
                9,
                WorkflowEvent::StepCompleted {
                    sequence: 5,
                    step_name: "increment".into(),
                },
            )
            .unwrap();

        let all = read_durable_changes(&store, 9, None);
        assert_eq!(all.len(), 3);

        let resumed = read_durable_changes(&store, 9, Some(all[0].cursor));
        assert_eq!(resumed.len(), 2);
        assert!(matches!(resumed[0].record, DurableChangeRecord::Event(_)));
        assert!(matches!(
            resumed[1].record,
            DurableChangeRecord::Workflow(_)
        ));
    }

    #[test]
    fn durable_change_json_uses_stable_wire_names() {
        let change = DurableChange {
            actor_id: 12,
            cursor: DurableChangeCursor {
                sequence: 4,
                lane: DurableChangeLane::Event,
                ordinal: 0,
            },
            record: DurableChangeRecord::Event(EventEntry {
                sequence: 4,
                field_name: "count".into(),
                event_name: "Incremented".into(),
                args: vec![],
                value: PersistedValue::Int(1),
            }),
        };

        let json = serde_json::to_value(change).unwrap();
        assert_eq!(json["cursor"]["lane"], "event");
        assert_eq!(json["record"]["kind"], "event");
    }

    #[test]
    fn stream_is_actor_scoped() {
        let mut store = MemoryStore::new();
        for actor_id in [1, 2] {
            store
                .append_journal(
                    actor_id,
                    JournalEntry {
                        sequence: 1,
                        behavior_id: 0,
                        payload: vec![],
                    },
                )
                .unwrap();
        }

        let changes = read_durable_changes(&store, 1, None);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].actor_id, 1);
    }
}
