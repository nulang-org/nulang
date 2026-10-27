//! Manual, isolated compatibility gate for replacing libSQL's local SQLite
//! driver. This does NOT migrate cloud URLs, credentials or production data.
use nulang::runtime::{
    ActorSnapshot, DurableTransition, JournalEntry, LibsqlStore, PersistedValue, PersistenceStore,
    DURABLE_TRANSITION_VERSION,
};
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
        workflow_events: vec![],
        domain_events: vec![],
        durable_effects: vec![],
        outbox: vec![],
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

        for table in ["durable_tails", "durable_transitions"] {
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

    let journal = legacy.read_journal(17);
    assert_eq!(journal.len(), 2);
    assert_eq!(journal[0].sequence, 1);
    assert_eq!(journal[1].sequence, 2);
    assert_eq!(journal[1].behavior_id, 3);
    assert_eq!(journal[1].payload, vec![PersistedValue::Int(10)]);
    println!(
        "PASS: actual Nulang LibsqlStore <-> Turso snapshot/journal, atomic-tail, and rollback compatibility"
    );
    Ok(())
}
