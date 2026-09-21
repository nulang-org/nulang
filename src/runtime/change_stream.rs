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
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
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
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
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

/// Read a bounded batch of an actor's durable history.
///
/// Each persistence backend receives an inclusive per-lane starting sequence
/// and may satisfy the range scan without materializing the actor's complete
/// history. The composite cursor is filtered after the lane scans so records
/// from later lanes at the same actor sequence are preserved.
///
/// after is exclusive. limit bounds the returned batch; zero performs no
/// storage reads.
pub fn scan_durable_changes(
    store: &dyn PersistenceStore,
    actor_id: u64,
    after: Option<DurableChangeCursor>,
    limit: usize,
) -> std::io::Result<Vec<DurableChange>> {
    if limit == 0 {
        return Ok(Vec::new());
    }

    let lane_window = |lane: DurableChangeLane| -> (u64, usize) {
        match after {
            None => (0, limit),
            Some(cursor) if lane < cursor.lane => (cursor.sequence.saturating_add(1), limit),
            Some(cursor) if lane == cursor.lane => (
                cursor.sequence,
                limit.saturating_add(cursor.ordinal.saturating_add(1) as usize),
            ),
            Some(cursor) => (cursor.sequence, limit),
        }
    };

    let (journal_start, journal_limit) = lane_window(DurableChangeLane::Journal);
    let (event_start, event_limit) = lane_window(DurableChangeLane::Event);
    let (workflow_start, workflow_limit) = lane_window(DurableChangeLane::Workflow);

    let journals = store.scan_journal_from(actor_id, journal_start, journal_limit);
    let events = store.scan_events_from(actor_id, event_start, event_limit);
    let workflows = store.scan_workflow_events_from(actor_id, workflow_start, workflow_limit);

    let mut changes = Vec::with_capacity(
        journals
            .len()
            .saturating_add(events.len())
            .saturating_add(workflows.len()),
    );

    let mut previous_sequence = None;
    let mut ordinal = 0u64;
    for entry in journals {
        if previous_sequence == Some(entry.sequence) {
            ordinal += 1;
        } else {
            previous_sequence = Some(entry.sequence);
            ordinal = 0;
        }
        changes.push(DurableChange {
            actor_id,
            cursor: DurableChangeCursor {
                sequence: entry.sequence,
                lane: DurableChangeLane::Journal,
                ordinal,
            },
            record: DurableChangeRecord::Journal(entry),
        });
    }

    previous_sequence = None;
    ordinal = 0;
    for entry in events {
        if previous_sequence == Some(entry.sequence) {
            ordinal += 1;
        } else {
            previous_sequence = Some(entry.sequence);
            ordinal = 0;
        }
        changes.push(DurableChange {
            actor_id,
            cursor: DurableChangeCursor {
                sequence: entry.sequence,
                lane: DurableChangeLane::Event,
                ordinal,
            },
            record: DurableChangeRecord::Event(entry),
        });
    }

    previous_sequence = None;
    ordinal = 0;
    for entry in workflows {
        let sequence = entry.sequence();
        if previous_sequence == Some(sequence) {
            ordinal += 1;
        } else {
            previous_sequence = Some(sequence);
            ordinal = 0;
        }
        changes.push(DurableChange {
            actor_id,
            cursor: DurableChangeCursor {
                sequence,
                lane: DurableChangeLane::Workflow,
                ordinal,
            },
            record: DurableChangeRecord::Workflow(entry),
        });
    }

    changes.sort_by_key(|change| change.cursor);
    if let Some(cursor) = after {
        changes.retain(|change| change.cursor > cursor);
    }
    changes.truncate(limit);
    Ok(changes)
}

/// Read all currently available durable history.
///
/// This compatibility helper is intentionally unbounded. New streaming,
/// projection, CDC, and analytical consumers should prefer scan_durable_changes
/// and persist the returned cursor.
pub fn read_durable_changes(
    store: &dyn PersistenceStore,
    actor_id: u64,
    after: Option<DurableChangeCursor>,
) -> Vec<DurableChange> {
    scan_durable_changes(store, actor_id, after, usize::MAX).unwrap_or_default()
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
            changes
                .iter()
                .map(DurableChange::sequence)
                .collect::<Vec<_>>(),
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
    fn bounded_scan_resumes_inside_same_sequence_event_batch() {
        let mut store = MemoryStore::new();

        for (field_name, value) in [("a", 1), ("b", 2), ("c", 3)] {
            store
                .append_event(
                    21,
                    EventEntry {
                        sequence: 8,
                        field_name: field_name.into(),
                        event_name: "Updated".into(),
                        args: vec![],
                        value: PersistedValue::Int(value),
                    },
                )
                .unwrap();
        }

        let first = scan_durable_changes(&store, 21, None, 2).unwrap();
        assert_eq!(first.len(), 2);
        assert_eq!(first[0].cursor.ordinal, 0);
        assert_eq!(first[1].cursor.ordinal, 1);

        let second = scan_durable_changes(&store, 21, Some(first[1].cursor), 2).unwrap();
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].cursor.sequence, 8);
        assert_eq!(second[0].cursor.ordinal, 2);
        match &second[0].record {
            DurableChangeRecord::Event(event) => assert_eq!(event.field_name, "c"),
            other => panic!("expected event, got {other:?}"),
        }
    }

    #[test]
    fn bounded_scan_preserves_later_lane_at_same_sequence() {
        let mut store = MemoryStore::new();
        store
            .append_journal(
                22,
                JournalEntry {
                    sequence: 5,
                    behavior_id: 7,
                    payload: vec![],
                },
            )
            .unwrap();
        store
            .append_event(
                22,
                EventEntry {
                    sequence: 5,
                    field_name: "count".into(),
                    event_name: "Incremented".into(),
                    args: vec![],
                    value: PersistedValue::Int(1),
                },
            )
            .unwrap();

        let first = scan_durable_changes(&store, 22, None, 1).unwrap();
        assert!(matches!(first[0].record, DurableChangeRecord::Journal(_)));

        let second = scan_durable_changes(&store, 22, Some(first[0].cursor), 1).unwrap();
        assert!(matches!(second[0].record, DurableChangeRecord::Event(_)));
        assert_eq!(second[0].cursor.sequence, 5);
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
