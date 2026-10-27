//! Manual, isolated compatibility gate for replacing libSQL's local SQLite
//! driver. This does NOT migrate cloud URLs, credentials or production data.
use nulang::durable_effect::{DurableEffectId, DurableEffectRecord, DurableEffectSpec};
use nulang::durable_effect_persistence::DurableEffectPersistenceRecord;
use nulang::primitives::{DeliverySemantics, EffectBoundary};
use nulang::runtime::{
    ActorSnapshot, DurableOutboxMessage, DurableTransition, EventEntry, JournalEntry, LibsqlStore,
    PersistedValue, PersistenceStore, WorkflowActivationId, WorkflowEvent,
    DURABLE_TRANSITION_VERSION,
};
use nulang::semantic_identity::{effect_site_id, EffectSiteOwnerKind};
use std::{collections::HashMap, error::Error};

fn main() -> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let db_file = temp.path().join("durable.db");

    // Nulang must produce the fixture through its REAL production store.
    {
        let mut legacy = LibsqlStore::new(&db_file)?;
        let mut state = HashMap::new();
        state.insert("count".to_owned(), PersistedValue::Int(42));
        legacy.save_snapshot(ActorSnapshot {
            actor_id: 17,
            sequence: 1,
            activation_epoch: 1,
            state,
            waiting_signal: None,
            crdt_snapshot: None,
            crdt_field_map: None,
            schema_name: None,
            authority_tokens: Default::default(),
        })?;
        legacy.append_journal(
            17,
            JournalEntry {
                sequence: 1,
                behavior_id: 2,
                payload: vec![PersistedValue::Int(10)],
            },
        )?;
    }

    // Exercise the actual atomic durable-transition/fencing contract before
    // switching readers, rather than synthesizing look-alike SQL rows.
    let durable_actor = 29;
    let effect_site = effect_site_id(
        "turso-schema-compat",
        EffectSiteOwnerKind::Behavior,
        "MigrationProbe.run",
        "Provider.ask",
        0,
    );
    let effect_id = DurableEffectId::derive_from_site(durable_actor, "turn:1", effect_site, 0);
    let effect = DurableEffectPersistenceRecord::from_effect(DurableEffectRecord::prepare(
        DurableEffectSpec::new(
            effect_id,
            "Provider.ask",
            EffectBoundary::External,
            DeliverySemantics::AtLeastOnce,
        ),
        b"migration-probe-request",
    ));
    let committed = DurableTransition {
        version: DURABLE_TRANSITION_VERSION,
        actor_id: durable_actor,
        activation_epoch: 7,
        sequence: 1,
        expected_previous_sequence: 0,
        command: Some(JournalEntry {
            sequence: 1,
            behavior_id: 6,
            payload: vec![PersistedValue::Int(99)],
        }),
        snapshot: Some(ActorSnapshot {
            actor_id: durable_actor,
            sequence: 1,
            activation_epoch: 7,
            ..ActorSnapshot::default()
        }),
        workflow_events: vec![WorkflowEvent::StepCompleted {
            sequence: 1,
            activation: Some(WorkflowActivationId::new(durable_actor, 1)),
            step_name: "step-migrated".to_owned(),
        }],
        domain_events: vec![EventEntry {
            sequence: 1,
            field_name: "count".to_owned(),
            event_name: "Incremented".to_owned(),
            args: vec![PersistedValue::Int(99)],
            value: PersistedValue::Int(100),
        }],
        durable_effects: vec![effect.clone()],
        outbox: vec![DurableOutboxMessage {
            destination_actor_id: 30,
            ordinal: 0,
            behavior_id: 12,
            payload: vec![PersistedValue::Int(99)],
        }],
    };
    {
        let mut legacy = LibsqlStore::new(&db_file)?;
        legacy.commit_transition(committed.clone())?;
        // Exactly the same durable message must be an idempotent replay.
        legacy.commit_transition(committed.clone())?;
        let mut conflict = committed.clone();
        conflict.command.as_mut().unwrap().payload = vec![PersistedValue::Int(100)];
        assert!(
            legacy.commit_transition(conflict).is_err(),
            "conflicting digest at the same epoch/sequence must be rejected"
        );
        let stale = DurableTransition {
            activation_epoch: 6,
            sequence: 2,
            expected_previous_sequence: 1,
            snapshot: None,
            command: None,
            ..committed.clone()
        };
        assert!(
            legacy.commit_transition(stale).is_err(),
            "lower activation epoch must never advance a committed tail"
        );
        let tail = legacy
            .load_durable_tail_position(durable_actor)?
            .ok_or("committed tail missing before migration")?;
        assert_eq!((tail.activation_epoch, tail.sequence), (7, 1));
    }

    // Current Turso is not the same API as the existing remote libSQL path.
    // Test local on-disk compatibility only. No remote requests are made.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async {
        let filename = db_file.to_str().ok_or("non-UTF8 temp filename")?;
        let db = turso::Builder::new_local(filename).build().await?;
        let conn = db.connect()?;

        let mut snapshot_rows = conn
            .query(
                "SELECT sequence, activation_epoch, state FROM snapshots WHERE actor_id=17",
                (),
            )
            .await?;
        let snapshot = snapshot_rows.next().await?.ok_or("snapshot not found")?;
        let seq: i64 = snapshot.get(0)?;
        let epoch: i64 = snapshot.get(1)?;
        let state: String = snapshot.get(2)?;
        assert_eq!((seq, epoch), (1, 1), "snapshot ordering/fence differs");
        assert!(
            state.contains("count") && state.contains("42"),
            "snapshot JSON corrupted"
        );
        drop(snapshot_rows);

        let mut tails = conn
            .query(
                "SELECT activation_epoch, sequence, digest FROM durable_tails WHERE actor_id=29",
                (),
            )
            .await?;
        let tail = tails
            .next()
            .await?
            .ok_or("Turso cannot see committed durable tail")?;
        let tail_epoch: i64 = tail.get(0)?;
        let tail_seq: i64 = tail.get(1)?;
        let digest: String = tail.get(2)?;
        assert_eq!((tail_epoch, tail_seq), (7, 1));
        assert_eq!(digest.len(), 64, "digest must retain full SHA/BLAKE hash");
        assert!(tails.next().await?.is_none(), "one committed tail expected");
        drop(tails);

        let mut transitions = conn
            .query(
                "SELECT activation_epoch, sequence, expected_previous_sequence
             FROM durable_transitions WHERE actor_id=29",
                (),
            )
            .await?;
        let row = transitions
            .next()
            .await?
            .ok_or("Turso cannot see atomic transition")?;
        let epoch: i64 = row.get(0)?;
        let sequence: i64 = row.get(1)?;
        let previous: i64 = row.get(2)?;
        assert_eq!((epoch, sequence, previous), (7, 1, 0));
        assert!(
            transitions.next().await?.is_none(),
            "duplicate/conflict wrote an extra transition"
        );
        drop(transitions);

        // Every committed transition component must survive a driver swap,
        // not just tail metadata. The RED probe failed when these were empty.
        for table in [
            "durable_workflow_events",
            "durable_domain_events",
            "durable_effect_records",
            "durable_outbox",
        ] {
            let query = format!("SELECT COUNT(*) FROM {table} WHERE actor_id=29 AND sequence=1");
            let mut rows = conn.query(&query, ()).await?;
            let count: i64 = rows
                .next()
                .await?
                .ok_or("missing durable row count")?
                .get(0)?;
            assert_eq!(count, 1, "Turso lost the committed {table} record");
        }

        let mut workflow_rows = conn
            .query(
                "SELECT ordinal, event FROM durable_workflow_events WHERE actor_id=29 AND sequence=1",
                (),
            )
            .await?;
        let workflow = workflow_rows.next().await?.ok_or("missing workflow event")?;
        assert_eq!(workflow.get::<i64>(0)?, 0);
        let workflow_json: String = workflow.get(1)?;
        assert!(workflow_json.contains("step-migrated"), "workflow payload drift");
        drop(workflow_rows);

        let mut domain_rows = conn
            .query(
                "SELECT ordinal, event FROM durable_domain_events WHERE actor_id=29 AND sequence=1",
                (),
            )
            .await?;
        let domain = domain_rows.next().await?.ok_or("missing domain event")?;
        assert_eq!(domain.get::<i64>(0)?, 0);
        let domain_json: String = domain.get(1)?;
        assert!(domain_json.contains("Incremented"), "domain event payload drift");
        drop(domain_rows);

        let mut effect_rows = conn
            .query(
                "SELECT ordinal, record FROM durable_effect_records WHERE actor_id=29 AND sequence=1",
                (),
            )
            .await?;
        let recorded = effect_rows.next().await?.ok_or("missing durable effect")?;
        assert_eq!(recorded.get::<i64>(0)?, 0);
        let effect_json: String = recorded.get(1)?;
        assert_eq!(
            DurableEffectPersistenceRecord::from_json(effect_json.as_bytes())?,
            effect,
            "durable effect identity/status changed across driver"
        );
        drop(effect_rows);

        let mut outbox_rows = conn
            .query(
                "SELECT ordinal, destination_actor_id, behavior_id, payload
                 FROM durable_outbox WHERE actor_id=29 AND sequence=1",
                (),
            )
            .await?;
        let outgoing = outbox_rows.next().await?.ok_or("missing outbox")?;
        assert_eq!(outgoing.get::<i64>(0)?, 0);
        assert_eq!(outgoing.get::<i64>(1)?, 30);
        assert_eq!(outgoing.get::<i64>(2)?, 12);
        let outbox_payload: String = outgoing.get(3)?;
        assert_eq!(outbox_payload, r#"[{"tag":"Int","value":99}]"#);
        drop(outbox_rows);

        let mut journal_rows = conn
            .query(
                "SELECT payload FROM journal WHERE actor_id=17 AND sequence=1",
                (),
            )
            .await?;
        let row = journal_rows.next().await?.ok_or("journal not found")?;
        let payload: String = row.get(0)?;
        drop(journal_rows);

        // Fault injection: simulate a constraint violation halfway through
        // a Turso write transaction touching *two* durable tables. A failed
        // transition must not leave a visible tail or partial transition.
        // This tests SQLite rollback behavior only, not Nulang's full CAS or
        // semantic digest validation.
        conn.execute("BEGIN IMMEDIATE", ()).await?;
        conn.execute(
            "INSERT INTO durable_tails
             (actor_id, activation_epoch, sequence, digest)
             VALUES (31, 4, 1, 'probe-only-not-a-valid-durable-digest')",
            (),
        )
        .await?;
        conn.execute(
            "INSERT INTO durable_transitions
             (actor_id, sequence, activation_epoch, expected_previous_sequence, digest)
             VALUES (31, 1, 4, 0, 'probe-only-not-a-valid-durable-digest')",
            (),
        )
        .await?;
        // A rollback must discard every sibling effect/event/outbox record;
        // a tail-only rollback would still leave replay-visible side effects.
        conn.execute(
            "INSERT INTO durable_workflow_events (actor_id, sequence, ordinal, event)
             VALUES (31, 1, 0, 'probe-rollback-only')",
            (),
        )
        .await?;
        conn.execute(
            "INSERT INTO durable_domain_events (actor_id, sequence, ordinal, event)
             VALUES (31, 1, 0, 'probe-rollback-only')",
            (),
        )
        .await?;
        conn.execute(
            "INSERT INTO durable_effect_records (actor_id, sequence, ordinal, record)
             VALUES (31, 1, 0, 'probe-rollback-only')",
            (),
        )
        .await?;
        conn.execute(
            "INSERT INTO durable_outbox
             (actor_id, sequence, ordinal, destination_actor_id, behavior_id, payload)
             VALUES (31, 1, 0, 32, 9, 'probe-rollback-only')",
            (),
        )
        .await?;
        let injected_failure = conn
            .execute(
                "INSERT INTO durable_transitions
             (actor_id, sequence, activation_epoch, expected_previous_sequence, digest)
             VALUES (31, 1, 4, 0, 'duplicate-should-fail')",
                (),
            )
            .await;
        assert!(injected_failure.is_err(), "primary-key collision must fail");
        conn.execute("ROLLBACK", ()).await?;

        for table in [
            "durable_tails",
            "durable_transitions",
            "durable_workflow_events",
            "durable_domain_events",
            "durable_effect_records",
            "durable_outbox",
        ] {
            let query = format!("SELECT COUNT(*) FROM {table} WHERE actor_id=31");
            let mut rows = conn.query(&query, ()).await?;
            let row = rows.next().await?.ok_or("missing count result")?;
            let count: i64 = row.get(0)?;
            assert_eq!(count, 0, "partial {table} write survived rollback");
        }

        // Turso writes the SAME serialized legacy journal payload, and
        // libSQL must be able to read it back, including behavior ID.
        conn.execute(
            "INSERT INTO journal (actor_id, sequence, behavior_id, payload)
             VALUES (17, 2, 3, ?1)",
            [payload],
        )
        .await?;
        drop(conn);
        drop(db);
        Ok::<(), Box<dyn Error>>(())
    })?;
    drop(rt);

    let legacy = LibsqlStore::new(&db_file)?;
    assert!(
        legacy.load_durable_tail_position(31)?.is_none(),
        "failed Turso transaction leaked a committed durable tail"
    );
    let snapshot = legacy.load_snapshot(17).ok_or("legacy snapshot lost")?;
    assert_eq!(snapshot.sequence, 1);
    assert_eq!(snapshot.activation_epoch, 1);
    assert_eq!(snapshot.state.get("count"), Some(&PersistedValue::Int(42)));
    let durable_tail = legacy
        .load_durable_tail_position(durable_actor)?
        .ok_or("durable tail missing after driver switch")?;
    assert_eq!(
        (durable_tail.activation_epoch, durable_tail.sequence),
        (7, 1)
    );

    assert_eq!(legacy.read_workflow_events(durable_actor).len(), 1);
    assert_eq!(legacy.read_events(durable_actor).len(), 1);
    assert_eq!(
        legacy
            .load_durable_effect(durable_actor, effect_id)?
            .ok_or("legacy effect lost")?,
        effect,
        "legacy store cannot recover the original durable effect"
    );

    let journal = legacy.read_journal(17);
    assert_eq!(journal.len(), 2);
    assert_eq!(journal[0].sequence, 1);
    assert_eq!(journal[1].sequence, 2);
    assert_eq!(journal[1].behavior_id, 3);
    assert_eq!(journal[1].payload, vec![PersistedValue::Int(10)]);
    println!(
        "PASS: actual Nulang LibsqlStore <-> Turso snapshot/journal, full transition records, and six-table rollback compatibility"
    );
    Ok(())
}
