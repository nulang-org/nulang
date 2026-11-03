//! Black-box compatibility gates for the experimental Turso transition writer.
//! Never run against a user database or an authenticated remote Turso instance.
use crate::turso_writer::commit_transition;
use nulang::runtime::{
    ActorSnapshot, DurableOutboxMessage, DurableTransition, JournalEntry, LibsqlStore,
    PersistedValue, PersistenceStore, DURABLE_TRANSITION_VERSION,
};
use std::{collections::HashMap, error::Error, path::Path};

fn transition(actor_id: u64, epoch: u64, sequence: u64, value: i64) -> DurableTransition {
    let mut state = HashMap::new();
    state.insert("count".to_owned(), PersistedValue::Int(value));
    DurableTransition {
        version: DURABLE_TRANSITION_VERSION,
        actor_id,
        activation_epoch: epoch,
        sequence,
        expected_previous_sequence: sequence - 1,
        command: Some(JournalEntry {
            sequence,
            behavior_id: 7,
            payload: vec![PersistedValue::Int(value)],
        }),
        snapshot: Some(ActorSnapshot {
            actor_id,
            sequence,
            activation_epoch: epoch,
            state,
            ..ActorSnapshot::default()
        }),
        workflow_events: vec![],
        domain_events: vec![],
        durable_effects: vec![],
        outbox: vec![DurableOutboxMessage {
            destination_actor_id: 100,
            ordinal: 0,
            behavior_id: 8,
            payload: vec![PersistedValue::Int(value)],
        }],
    }
}

pub(crate) fn verify_turso_transition_writer(db_path: &Path) -> Result<(), Box<dyn Error>> {
    // The test-only DB schema is created by the real legacy Nulang store.
    let _legacy = LibsqlStore::new(db_path)?;
    drop(_legacy);
    let filename = db_path.to_str().ok_or("non-UTF8 test database filename")?;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async {
        let db = turso::Builder::new_local(filename).build().await?;
        let conn = db.connect()?;

        let original = transition(52, 9, 1, 10);
        let original_digest = original.digest()?;
        let receipt = commit_transition(&conn, original.clone()).await?;
        assert_eq!(receipt.digest, original_digest, "must use Nulang canonical digest");
        assert_eq!(
            commit_transition(&conn, original.clone()).await?.digest,
            original_digest,
            "same transition must replay idempotently"
        );
        let mut collision = original.clone();
        collision.command.as_mut().unwrap().payload = vec![PersistedValue::Int(999)];
        assert!(
            commit_transition(&conn, collision).await.is_err(),
            "same epoch/sequence with changed payload must conflict"
        );

        let newer = transition(52, 10, 2, 11);
        commit_transition(&conn, newer.clone()).await?;
        assert!(
            commit_transition(&conn, transition(52, 9, 3, 12)).await.is_err(),
            "older activation epoch must be fenced"
        );
        let mut wrong_predecessor = transition(52, 10, 4, 13);
        wrong_predecessor.expected_previous_sequence = 3;
        assert!(
            commit_transition(&conn, wrong_predecessor).await.is_err(),
            "a transition must extend exactly the committed tail"
        );
        let mut invalid = transition(52, 10, 3, 12);
        invalid.command.as_mut().unwrap().sequence = 123;
        assert!(
            commit_transition(&conn, invalid).await.is_err(),
            "structurally invalid commands must be rejected before writing"
        );

        // Two DISTINCT connections race to acquire the same first tail for a
        // new actor with two different canonical digests.
        let contender_a = db.connect()?;
        let contender_b = db.connect()?;
        let (a, b) = tokio::join!(
            commit_transition(&contender_a, transition(53, 1, 1, 201)),
            commit_transition(&contender_b, transition(53, 1, 1, 202))
        );
        assert_eq!(
            usize::from(a.is_ok()) + usize::from(b.is_ok()), 1,
            "concurrent conflicting writers must have exactly one winner: a={a:?}, b={b:?}"
        );

        // One atomic receipt, one command, one outbox row per committed turn.
        for (table, actor, expected) in [
            ("durable_transitions", 52, 2),
            ("durable_transitions", 53, 1),
            ("journal", 52, 2),
            ("journal", 53, 1),
            ("durable_outbox", 52, 2),
            ("durable_outbox", 53, 1),
        ] {
            let sql = format!("SELECT COUNT(*) FROM {table} WHERE actor_id=?1");
            let mut rows = conn.query(&sql, [actor]).await?;
            let count: i64 = rows.next().await?.ok_or("missing count")?.get(0)?;
            assert_eq!(count, expected, "unexpected {table} count for actor {actor}");
        }

        let mut rows = conn
            .query(
                "SELECT activation_epoch, sequence, digest FROM durable_tails WHERE actor_id=52",
                (),
            )
            .await?;
        let tail = rows.next().await?.ok_or("missing actor 52 tail")?;
        let epoch: i64 = tail.get(0)?;
        let seq: i64 = tail.get(1)?;
        let digest: String = tail.get(2)?;
        assert_eq!((epoch, seq), (10, 2));
        assert_eq!(digest, hex_digest(&newer.digest()?));
        drop(rows);
        drop(contender_a);
        drop(contender_b);
        drop(conn);
        drop(db);
        Ok::<(), Box<dyn Error>>(())
    })?;
    drop(rt);

    // Read real Turso-authored records through the existing production reader.
    let mut legacy = LibsqlStore::new(db_path)?;
    let tail = legacy
        .load_durable_tail_position(52)?
        .ok_or("existing reader cannot see Turso committed tail")?;
    assert_eq!((tail.activation_epoch, tail.sequence), (10, 2));
    let snapshot = legacy.load_snapshot(52).ok_or("Turso snapshot unreadable")?;
    assert_eq!(snapshot.sequence, 2);
    assert_eq!(snapshot.state.get("count"), Some(&PersistedValue::Int(11)));
    let commands = legacy.read_journal(52);
    assert_eq!(commands.len(), 2);
    assert_eq!(commands[0].payload, vec![PersistedValue::Int(10)]);
    assert_eq!(commands[1].payload, vec![PersistedValue::Int(11)]);
    assert!(
        legacy.commit_transition(transition(52, 9, 3, 12)).is_err(),
        "legacy writer must reject stale epoch after Turso commit"
    );
    legacy.commit_transition(transition(52, 11, 3, 12))?;
    assert_eq!(
        legacy.load_durable_tail_position(52)?.unwrap().sequence, 3,
        "Nulang's writer must advance the Turso-created tail"
    );
    println!("PASS: test-only Turso transition commits/replays/conflicts/fences, concurrent writer, cross-driver read/write");
    Ok(())
}

fn hex_digest(digest: &[u8; 32]) -> String {
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}
