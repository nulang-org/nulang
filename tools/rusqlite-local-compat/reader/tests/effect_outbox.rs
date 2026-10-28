//! Exercise typed effect/outbox reads against existing libSQL schema.
use nulang::{
    durable_effect::{DurableEffectId, DurableEffectRecord, DurableEffectSpec},
    durable_effect_persistence::DurableEffectPersistenceRecord,
    primitives::{DeliverySemantics, EffectBoundary},
    runtime::{
        ActorSnapshot, DurableOutboxMessage, DurableTransition, JournalEntry, PersistedValue,
        DURABLE_TRANSITION_VERSION,
    },
};
use nulang_rusqlite_recovery_reader::{RusqliteAtomicWriter, RusqliteRecoveryReader};
use rusqlite::Connection;
use std::{
    fs,
    io::ErrorKind,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

fn fixture(label: &str) -> (PathBuf, PathBuf) {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "nulang-effect-outbox-{label}-{}-{stamp}",
        std::process::id()
    ));
    fs::create_dir(&dir).unwrap();
    let path = dir.join("old.db");
    fs::copy(std::env::var_os("NULANG_LEGACY_FIXTURE_DB").unwrap(), &path).unwrap();
    (dir, path)
}
fn effect_id() -> DurableEffectId {
    DurableEffectId::derive(42, "checkout-42", 0, "Inference.ask")
}
fn sample_effect() -> DurableEffectRecord {
    let spec = DurableEffectSpec::new(
        effect_id(),
        "Inference.ask",
        EffectBoundary::External,
        DeliverySemantics::EffectivelyOnceWithDeduplication,
    );
    DurableEffectRecord::prepare(spec, b"request-42")
}
fn committed_effect_transition(
    sequence: u64,
    last: u64,
    effect: DurableEffectRecord,
    tag: &str,
) -> DurableTransition {
    DurableTransition {
        version: DURABLE_TRANSITION_VERSION,
        actor_id: 42,
        activation_epoch: 1,
        sequence,
        expected_previous_sequence: last,
        command: Some(JournalEntry {
            sequence,
            behavior_id: 3,
            payload: vec![PersistedValue::String(tag.to_owned())],
        }),
        snapshot: Some(ActorSnapshot {
            actor_id: 42,
            sequence,
            activation_epoch: 1,
            state: [("tag".to_string(), PersistedValue::String(tag.to_owned()))].into(),
            ..ActorSnapshot::default()
        }),
        workflow_events: vec![],
        domain_events: vec![],
        durable_effects: vec![DurableEffectPersistenceRecord::from_effect(effect)],
        outbox: vec![
            DurableOutboxMessage {
                destination_actor_id: 86,
                ordinal: 2,
                behavior_id: 5,
                payload: vec![PersistedValue::String(tag.to_owned())],
            },
            DurableOutboxMessage {
                destination_actor_id: 85,
                ordinal: 0,
                behavior_id: 4,
                payload: vec![PersistedValue::Int(sequence as i64)],
            },
        ],
    }
}

#[test]
fn prepared_and_completed_records_recover_latest_identity_without_redispatch() {
    let (dir, path) = fixture("effect");
    let prepared = sample_effect();
    let completed = prepared.clone().complete(b"provider-result".to_vec());
    let mut writer = RusqliteAtomicWriter::open_existing(&path).unwrap();
    writer
        .commit(&committed_effect_transition(3, 2, prepared, "prepared"))
        .unwrap();
    writer
        .commit(&committed_effect_transition(
            4,
            3,
            completed.clone(),
            "completed",
        ))
        .unwrap();
    drop(writer);
    let reader = RusqliteRecoveryReader::open(&path).unwrap();
    let actual = reader
        .load_durable_effect(42, effect_id())
        .unwrap()
        .unwrap();
    assert_eq!(
        actual,
        DurableEffectPersistenceRecord::from_effect(completed)
    );
    assert!(reader
        .load_durable_effect(999, effect_id())
        .unwrap()
        .is_none());
    let unknown = DurableEffectId::derive(42, "other", 0, "Inference.ask");
    assert!(reader.load_durable_effect(42, unknown).unwrap().is_none());
    let messages = reader.outbox(42).unwrap();
    assert_eq!(messages.len(), 4);
    assert_eq!(
        messages
            .iter()
            .map(|x| (x.sequence, x.message.ordinal))
            .collect::<Vec<_>>(),
        vec![(3, 0), (3, 2), (4, 0), (4, 2)]
    );
    assert_eq!(messages[0].message.destination_actor_id, 85);
    assert_eq!(messages[1].message.destination_actor_id, 86);
    assert_eq!(messages[0].message.payload, vec![PersistedValue::Int(3)]);
    assert_eq!(messages[2].message.payload, vec![PersistedValue::Int(4)]);
    drop(reader);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn malformed_newer_effect_blocks_replay_instead_of_skipping_history() {
    let (dir, path) = fixture("corrupt-effect");
    let mut writer = RusqliteAtomicWriter::open_existing(&path).unwrap();
    writer
        .commit(&committed_effect_transition(
            3,
            2,
            sample_effect(),
            "prepared",
        ))
        .unwrap();
    writer
        .commit(&committed_effect_transition(
            4,
            3,
            sample_effect().complete(b"ok".to_vec()),
            "done",
        ))
        .unwrap();
    drop(writer);
    let c = Connection::open(&path).unwrap();
    c.execute(
        "UPDATE durable_effect_records SET record='{broken-json' WHERE actor_id=42 AND sequence=4",
        [],
    )
    .unwrap();
    drop(c);
    let reader = RusqliteRecoveryReader::open(&path).unwrap();
    assert_eq!(
        reader
            .load_durable_effect(42, effect_id())
            .unwrap_err()
            .kind(),
        ErrorKind::InvalidData
    );
    drop(reader);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn malformed_outbox_or_effect_identity_must_fail_closed() {
    let (dir, path) = fixture("outbox-corrupt");
    let mut writer = RusqliteAtomicWriter::open_existing(&path).unwrap();
    writer
        .commit(&committed_effect_transition(
            3,
            2,
            sample_effect(),
            "prepared",
        ))
        .unwrap();
    drop(writer);
    let c = Connection::open(&path).unwrap();
    c.execute("UPDATE durable_outbox SET payload='not-json' WHERE actor_id=42 AND sequence=3 AND ordinal=2",[]).unwrap();
    drop(c);
    let reader = RusqliteRecoveryReader::open(&path).unwrap();
    assert_eq!(
        reader.outbox(42).unwrap_err().kind(),
        ErrorKind::InvalidData
    );
    drop(reader);
    let c = Connection::open(&path).unwrap();
    c.execute("UPDATE durable_outbox SET payload='[]',destination_actor_id=-1 WHERE actor_id=42 AND sequence=3 AND ordinal=2",[]).unwrap();
    drop(c);
    let reader = RusqliteRecoveryReader::open(&path).unwrap();
    assert_eq!(
        reader.outbox(42).unwrap_err().kind(),
        ErrorKind::InvalidData
    );
    drop(reader);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn late_outbox_constraint_failure_rolls_back_effect_and_snapshot() {
    let (dir, path) = fixture("effect-rollback");
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch(
        "CREATE TRIGGER abort_effect_outbox BEFORE INSERT ON durable_outbox
        BEGIN SELECT RAISE(ABORT,'fault after effect staged'); END",
    )
    .unwrap();
    drop(conn);
    let mut writer = RusqliteAtomicWriter::open_existing(&path).unwrap();
    assert!(writer
        .commit(&committed_effect_transition(
            3,
            2,
            sample_effect(),
            "never-committed"
        ))
        .is_err());
    drop(writer);
    let reader = RusqliteRecoveryReader::open(&path).unwrap();
    assert_eq!(reader.tail(42).unwrap().unwrap().sequence, 2);
    assert!(reader
        .load_durable_effect(42, effect_id())
        .unwrap()
        .is_none());
    assert!(reader.outbox(42).unwrap().is_empty());
    assert_eq!(reader.snapshot(42).unwrap().unwrap().sequence, 2);
    assert_eq!(reader.journal(42).unwrap().len(), 2);
    drop(reader);
    let conn = Connection::open(&path).unwrap();
    for table in [
        "durable_effect_records",
        "durable_outbox",
        "durable_transitions",
        "durable_workflow_events",
        "durable_domain_events",
    ] {
        let sql = format!("SELECT COUNT(*) FROM {table} WHERE actor_id=42 AND sequence=3");
        assert_eq!(
            conn.query_row(&sql, [], |r| r.get::<_, i64>(0)).unwrap(),
            0,
            "partial {table}"
        );
    }
    fs::remove_dir_all(dir).unwrap();
}
