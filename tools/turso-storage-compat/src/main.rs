//! Manual, isolated compatibility gate for replacing libSQL's local SQLite
//! driver. This does NOT migrate cloud URLs, credentials or production data.
use nulang::runtime::{ActorSnapshot, JournalEntry, LibsqlStore, PersistedValue, PersistenceStore};
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

        let mut journal_rows = conn
            .query(
                "SELECT payload FROM journal WHERE actor_id=17 AND sequence=1",
                (),
            )
            .await?;
        let row = journal_rows.next().await?.ok_or("journal not found")?;
        let payload: String = row.get(0)?;
        drop(journal_rows);

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
    let snapshot = legacy.load_snapshot(17).ok_or("legacy snapshot lost")?;
    assert_eq!(snapshot.sequence, 1);
    assert_eq!(snapshot.activation_epoch, 1);
    assert_eq!(snapshot.state.get("count"), Some(&PersistedValue::Int(42)));
    let journal = legacy.read_journal(17);
    assert_eq!(journal.len(), 2);
    assert_eq!(journal[0].sequence, 1);
    assert_eq!(journal[1].sequence, 2);
    assert_eq!(journal[1].behavior_id, 3);
    assert_eq!(journal[1].payload, vec![PersistedValue::Int(10)]);
    println!("PASS: actual Nulang LibsqlStore <-> Turso on-disk snapshot/journal compatibility");
    Ok(())
}
