//! Full durable-record parity gate for the isolated Turso compatibility writer.
//! Uses real Nulang event types, versioned effect envelopes, and the legacy reader.
use crate::turso_writer::commit_transition;
use nulang::durable_effect::{DurableEffectId, DurableEffectRecord, DurableEffectSpec};
use nulang::durable_effect_persistence::DurableEffectPersistenceRecord;
use nulang::primitives::{DeliverySemantics, EffectBoundary};
use nulang::runtime::{
    ActorSnapshot, DurableOutboxMessage, DurableTransition, EventEntry, JournalEntry, LibsqlStore,
    PersistedValue, PersistenceStore, WorkflowActivationId, WorkflowEvent,
    DURABLE_TRANSITION_VERSION,
};
use std::{collections::HashMap, error::Error, path::Path};

pub(crate) fn rich_transition(actor: u64) -> (DurableTransition, DurableEffectPersistenceRecord) {
    let sequence = 1;
    let spec = DurableEffectSpec::new(
        DurableEffectId::derive(actor, "probe:turn:1", 0, "Provider.ask"),
        "Provider.ask",
        EffectBoundary::External,
        DeliverySemantics::AtLeastOnce,
    );
    let prepared = DurableEffectRecord::prepare(spec, b"prompt=hello");
    let completed = DurableEffectPersistenceRecord::from_effect(
        prepared.clone().complete(b"answer=world".to_vec()),
    );
    let compensation = completed
        .effect()
        .prepare_compensation(
            0,
            "Provider.undo",
            EffectBoundary::External,
            DeliverySemantics::EffectivelyOnceWithDeduplication,
            b"undo=request",
        )
        .expect("completed effect must allow compensation");
    let compensation = DurableEffectPersistenceRecord::from_compensation(&compensation);
    let mut state = HashMap::new();
    state.insert("counter".to_string(), PersistedValue::Int(9));
    (
        DurableTransition {
            version: DURABLE_TRANSITION_VERSION,
            actor_id: actor,
            activation_epoch: 4,
            sequence,
            expected_previous_sequence: 0,
            command: Some(JournalEntry {
                sequence,
                behavior_id: 8,
                payload: vec![PersistedValue::Int(9)],
            }),
            snapshot: Some(ActorSnapshot {
                actor_id: actor,
                sequence,
                activation_epoch: 4,
                state,
                ..ActorSnapshot::default()
            }),
            workflow_events: vec![
                WorkflowEvent::StepCompleted {
                    sequence,
                    activation: Some(WorkflowActivationId::new(actor, sequence)),
                    step_name: "effect".to_string(),
                },
                WorkflowEvent::Custom {
                    sequence,
                    replay_id: None,
                    name: "receipt".to_string(),
                    args: vec![PersistedValue::Int(9)],
                },
            ],
            domain_events: vec![
                EventEntry {
                    sequence,
                    field_name: "counter".to_string(),
                    event_name: "Changed".to_string(),
                    args: vec![PersistedValue::Int(1)],
                    value: PersistedValue::Int(9),
                },
                EventEntry {
                    sequence,
                    field_name: "mirror".to_string(),
                    event_name: "Changed".to_string(),
                    args: vec![],
                    value: PersistedValue::Int(9),
                },
            ],
            durable_effects: vec![
                DurableEffectPersistenceRecord::from_effect(prepared),
                completed.clone(),
                compensation,
            ],
            outbox: vec![DurableOutboxMessage {
                destination_actor_id: actor + 1,
                ordinal: 0,
                behavior_id: 2,
                payload: vec![PersistedValue::Int(9)],
            }],
        },
        completed,
    )
}

pub(crate) fn verify_full_record_parity(path: &Path) -> Result<(), Box<dyn Error>> {
    // Schema is always created by the real Nulang implementation, not a copy.
    drop(LibsqlStore::new(path)?);
    let filename = path.to_str().ok_or("non-UTF8 test filename")?;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async {
        let db = turso::Builder::new_local(filename).build().await?;
        let conn = db.connect()?;
        let (rich, completed) = rich_transition(71);
        let compensation = rich.durable_effects[2].clone();
        let digest = rich.digest()?;
        assert_eq!(
            commit_transition(&conn, rich.clone()).await?.digest,
            digest,
            "record-rich transition must return Nulang's canonical digest"
        );
        assert_eq!(
            commit_transition(&conn, rich.clone()).await?.digest,
            digest,
            "identical retry must not duplicate any durable records"
        );
        let mut changed = rich.clone();
        changed.domain_events[0].event_name = "Tampered".to_string();
        assert!(
            commit_transition(&conn, changed).await.is_err(),
            "same-epoch tampered event must conflict"
        );

        let expected_workflow = rich
            .workflow_events
            .iter()
            .map(serde_json::to_string)
            .collect::<Result<Vec<_>, _>>()?;
        let expected_domain = rich
            .domain_events
            .iter()
            .map(serde_json::to_string)
            .collect::<Result<Vec<_>, _>>()?;
        let expected_effect = rich
            .durable_effects
            .iter()
            .map(|record| -> Result<String, Box<dyn Error>> {
                Ok(String::from_utf8(record.to_json()?)?)
            })
            .collect::<Result<Vec<_>, _>>()?;

        for (table, column, expected) in [
            ("durable_workflow_events", "event", &expected_workflow),
            ("durable_domain_events", "event", &expected_domain),
            ("durable_effect_records", "record", &expected_effect),
        ] {
            let query = format!(
                "SELECT ordinal,{column} FROM {table} WHERE actor_id=71 AND sequence=1 ORDER BY ordinal"
            );
            let mut rows = conn.query(&query, ()).await?;
            for (index, value) in expected.iter().enumerate() {
                let row = rows.next().await?.ok_or("missing durable record")?;
                let ordinal: i64 = row.get(0)?;
                let encoded: String = row.get(1)?;
                assert_eq!(ordinal, index as i64, "{table} ordinal mismatch");
                assert_eq!(&encoded, value, "{table} serialization mismatch");
            }
            assert!(
                rows.next().await?.is_none(),
                "{table} should have no records added by identical retry"
            );
        }

        // Force a late primary-key error, after all event/effect writes occur.
        // Any earlier committed records would violate one-transition atomicity.
        conn.execute(
            "INSERT INTO durable_outbox
             (actor_id,sequence,ordinal,destination_actor_id,behavior_id,payload)
             VALUES (72,1,0,999,1,'[]')",
            (),
        )
        .await?;
        let (will_fail, _) = rich_transition(72);
        assert!(
            commit_transition(&conn, will_fail).await.is_err(),
            "deliberate outbox PK conflict must abort the complete transition"
        );
        for table in [
            "durable_tails",
            "durable_transitions",
            "journal",
            "durable_workflow_events",
            "durable_domain_events",
            "durable_effect_records",
        ] {
            let query = format!("SELECT COUNT(*) FROM {table} WHERE actor_id=72");
            let mut rows = conn.query(&query, ()).await?;
            let count: i64 = rows.next().await?.ok_or("missing count")?.get(0)?;
            assert_eq!(count, 0, "{table} leaked a partial transaction");
        }
        let mut rows = conn
            .query("SELECT COUNT(*) FROM durable_outbox WHERE actor_id=72", ())
            .await?;
        let count: i64 = rows.next().await?.ok_or("missing outbox count")?.get(0)?;
        assert_eq!(count, 1, "seed row must remain after failed commit");
        drop(rows);
        drop(conn);
        drop(db);
        Ok::<_, Box<dyn Error>>((completed, compensation))
    })
    .and_then(|(completed, compensation)| {
        drop(rt);
        let mut legacy = LibsqlStore::new(path)?;
        assert_eq!(legacy.read_workflow_events(71).len(), 2);
        assert_eq!(legacy.read_events(71).len(), 2);
        let effect_id = completed.effect().spec().id;
        assert_eq!(
            legacy.load_durable_effect(71, effect_id)?,
            Some(completed),
            "legacy reader must load newest Turso-written versioned effect"
        );
        assert_eq!(
            compensation.original_effect_id(),
            Some(effect_id),
            "compensation must preserve original effect identity"
        );
        assert_eq!(
            compensation.compensation_ordinal(),
            Some(0),
            "compensation must preserve its ordinal"
        );
        assert_eq!(
            legacy.load_durable_effect(71, compensation.effect().spec().id)?,
            Some(compensation),
            "legacy reader must recover Turso-authored compensation linkage"
        );
        assert!(
            legacy.load_snapshot(72).is_none(),
            "failed rich transition must not modify snapshot"
        );
        assert!(legacy.read_journal(72).is_empty());
        assert!(legacy.read_workflow_events(72).is_empty());
        assert!(legacy.read_events(72).is_empty());
        assert!(legacy.load_durable_tail_position(72)?.is_none());
        let (next, _) = rich_transition(73);
        // A complete record-rich transition must remain readable after a
        // Nulang writer commits on the same database.
        legacy.commit_transition(next)?;
        println!(
            "PASS: Turso workflow/domain/effect records, canonical encoding, retry, and mid-transaction rollback"
        );
        Ok::<(), Box<dyn Error>>(())
    })
}
