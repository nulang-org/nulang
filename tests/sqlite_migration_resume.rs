//! Disposable committed-prefix migration resumption tests for actor SQLite.
#![cfg(feature = "sqlite")]

use nulang::runtime::{ActorSnapshot, JournalEntry, LibsqlStore, PersistedValue, PersistenceStore};
use rusqlite::{params, Connection};
use std::{collections::HashMap, path::PathBuf, time::SystemTime};

const ACTOR: u64 = 17;
const COLUMNS: [(&str, &str); 6] = [
    (
        "activation_epoch",
        "activation_epoch INTEGER NOT NULL DEFAULT 1",
    ),
    ("waiting_signal", "waiting_signal TEXT"),
    ("crdt_snapshot", "crdt_snapshot TEXT"),
    ("crdt_field_map", "crdt_field_map TEXT"),
    ("schema_name", "schema_name TEXT"),
    ("authority_tokens", "authority_tokens TEXT"),
];
const AUXILIARY: [&str; 8] = [
    "workflow_events",
    "events",
    "durable_tails",
    "durable_transitions",
    "durable_workflow_events",
    "durable_domain_events",
    "durable_effect_records",
    "durable_outbox",
];

struct Disposable(PathBuf);
impl Drop for Disposable {
    fn drop(&mut self) {
        // Only the directory created by this test is removed.
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn snapshot(prefix: usize, sequence: u64, count: i64) -> ActorSnapshot {
    let mut value = ActorSnapshot {
        actor_id: ACTOR,
        sequence,
        state: HashMap::from([
            ("count".into(), PersistedValue::Int(count)),
            ("label".into(), PersistedValue::String("legacy α".into())),
            ("optional".into(), PersistedValue::Nil),
        ]),
        ..Default::default()
    };
    if prefix >= 1 {
        value.activation_epoch = 3;
    }
    if prefix >= 2 {
        value.waiting_signal = Some("approved".into());
    }
    if prefix >= 3 {
        value.crdt_snapshot = Some(vec![(11, 0, vec![1, 2, 3])]);
    }
    if prefix >= 4 {
        value.crdt_field_map = Some(HashMap::from([("counter".into(), 11)]));
    }
    if prefix >= 5 {
        value.schema_name = Some("fixture::Worker".into());
    }
    if prefix >= 6 {
        // Exercise opaque persisted authority metadata, not real authority.
        value.authority_tokens.insert("fixture:opaque".into());
    }
    value
}

fn assert_snapshot(store: &LibsqlStore, expected: &ActorSnapshot) {
    let actual = store
        .load_snapshot(ACTOR)
        .expect("snapshot must survive migration");
    assert_eq!(
        serde_json::to_value(actual).unwrap(),
        serde_json::to_value(expected).unwrap()
    );
}

#[test]
fn committed_migration_prefixes_reopen_without_losing_existing_state() {
    let nonce = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "nulang-sqlite-prefix-{}-{nonce}",
        std::process::id()
    ));
    std::fs::create_dir(&path).unwrap();
    let dir = Disposable(path);

    // The full candidate schema provides the exact table definitions for
    // auxiliary creation-prefix fixtures, avoiding a second copied schema.
    let reference_path = dir.0.join("reference.db");
    drop(LibsqlStore::new(&reference_path).unwrap());
    let reference = Connection::open(&reference_path).unwrap();
    let tables: Vec<String> = AUXILIARY
        .iter()
        .map(|name| {
            reference
                .query_row(
                    "SELECT sql FROM sqlite_master WHERE type='table' AND name=?1",
                    [name],
                    |row| row.get(0),
                )
                .unwrap()
        })
        .collect();
    drop(reference);

    // Every committed optional-column prefix, then every committed
    // auxiliary-table prefix after all optional columns exist.
    let cases = (0..=COLUMNS.len())
        .map(|prefix| (prefix, 0))
        .chain((1..=AUXILIARY.len()).map(|aux| (COLUMNS.len(), aux)));
    for (prefix, aux) in cases {
        let path = dir.0.join(format!("columns-{prefix}-tables-{aux}.db"));
        let expected = snapshot(prefix, 3, 7);
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE snapshots (actor_id INTEGER PRIMARY KEY, sequence INTEGER NOT NULL, state TEXT NOT NULL);
                 CREATE TABLE journal (actor_id INTEGER NOT NULL, sequence INTEGER NOT NULL, behavior_id INTEGER NOT NULL, payload TEXT NOT NULL, PRIMARY KEY(actor_id, sequence));",
            ).unwrap();
            conn.execute(
                "INSERT INTO snapshots (actor_id, sequence, state) VALUES (?1, 3, ?2)",
                params![
                    ACTOR as i64,
                    serde_json::to_string(&expected.state).unwrap()
                ],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO journal (actor_id, sequence, behavior_id, payload) VALUES (?1, 4, 2, ?2)",
                params![ACTOR as i64, serde_json::to_string(&vec![PersistedValue::Int(7)]).unwrap()],
            ).unwrap();
            for (_, definition) in &COLUMNS[..prefix] {
                conn.execute(
                    &format!("ALTER TABLE snapshots ADD COLUMN {definition}"),
                    [],
                )
                .unwrap();
            }
            let values = [
                expected.activation_epoch.to_string(),
                expected.waiting_signal.clone().unwrap_or_default(),
                serde_json::to_string(&expected.crdt_snapshot).unwrap(),
                serde_json::to_string(&expected.crdt_field_map).unwrap(),
                expected.schema_name.clone().unwrap_or_default(),
                serde_json::to_string(&expected.authority_tokens).unwrap(),
            ];
            for ((name, _), value) in COLUMNS[..prefix].iter().zip(values.iter()) {
                conn.execute(
                    &format!("UPDATE snapshots SET {name}=?1 WHERE actor_id=?2"),
                    params![value, ACTOR as i64],
                )
                .unwrap();
            }
            for sql in &tables[..aux] {
                conn.execute_batch(sql).unwrap();
            }
        }

        // Resume from the deliberately committed prefix; no live process
        // is killed here and no incomplete transaction is claimed.
        let mut store = LibsqlStore::new(&path)
            .unwrap_or_else(|error| panic!("resume prefix {prefix}/{aux}: {error}"));
        assert_snapshot(&store, &expected);
        let journal = store.read_journal(ACTOR);
        assert_eq!(journal.len(), 1);
        assert_eq!(journal[0].sequence, 4);
        assert_eq!(journal[0].payload, vec![PersistedValue::Int(7)]);
        let upgraded = snapshot(COLUMNS.len(), 5, 9);
        store.save_snapshot(upgraded.clone()).unwrap();
        store
            .append_journal(
                ACTOR,
                JournalEntry {
                    sequence: 6,
                    behavior_id: 3,
                    payload: vec![PersistedValue::Int(9)],
                },
            )
            .unwrap();
        drop(store);

        let store = LibsqlStore::new(&path).unwrap();
        assert_snapshot(&store, &upgraded);
        assert_eq!(store.latest_sequence(ACTOR), 6);
        let journal = store.read_journal(ACTOR);
        assert_eq!(journal.len(), 2);
        assert_eq!(journal[0].payload, vec![PersistedValue::Int(7)]);
        assert_eq!(journal[1].payload, vec![PersistedValue::Int(9)]);
        drop(store);

        let conn = Connection::open(&path).unwrap();
        let check: String = conn
            .query_row("PRAGMA integrity_check", [], |row| row.get(0))
            .unwrap();
        assert_eq!(check, "ok");
        for name in AUXILIARY {
            let present: i64 = conn
                .query_row(
                    "SELECT count(*) FROM sqlite_master WHERE type='table' AND name=?1",
                    [name],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(
                present, 1,
                "missing table {name} after prefix {prefix}/{aux}"
            );
        }
        println!("validated committed migration prefix: {prefix} optional columns, {aux} auxiliary tables");
    }
}

#[cfg(unix)]
const CHILD_DIR_ENV: &str = "NULANG_TEST_INTERRUPTED_DDL_FIXTURE_DIR";
#[cfg(unix)]
const CHILD_READY: &str = "NULANG_SQLITE_UNCOMMITTED_DDL_READY";

/// Subprocess fixture only. The parent owns the directory and supplies its
/// path; an ordinary test run without the parent environment opens no database.
#[cfg(unix)]
#[test]
fn uncommitted_ddl_child() {
    use std::io::Write;

    let Some(dir) = std::env::var_os(CHILD_DIR_ENV).map(PathBuf::from) else {
        return;
    };
    let dir = dir.canonicalize().expect("canonical fixture directory");
    assert_eq!(
        dir.parent(),
        Some(std::env::temp_dir().canonicalize().unwrap().as_path())
    );
    assert!(dir
        .file_name()
        .unwrap()
        .to_string_lossy()
        .starts_with("nulang-sqlite-interrupted-ddl-"));
    let path = dir.join("interrupted.db");
    assert!(std::fs::symlink_metadata(&path)
        .unwrap()
        .file_type()
        .is_file());
    assert_eq!(path.canonicalize().unwrap().parent(), Some(dir.as_path()));

    let conn =
        Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE).unwrap();
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA synchronous=FULL;
         PRAGMA cache_size=8;
         PRAGMA cache_spill=ON;
         BEGIN IMMEDIATE;
         ALTER TABLE snapshots ADD COLUMN activation_epoch INTEGER NOT NULL DEFAULT 1;
         UPDATE snapshots SET sequence=99, activation_epoch=99;
         CREATE TABLE uncommitted_spill_fixture(data BLOB);",
    )
    .unwrap();
    // Force dirty pages to the WAL without committing the transaction. An
    // acknowledged marker must not be mistaken for a commit acknowledgment.
    for _ in 0..4 {
        conn.execute(
            "INSERT INTO uncommitted_spill_fixture VALUES (zeroblob(1048576))",
            [],
        )
        .unwrap();
    }
    let observed: (i64, i64) = conn
        .query_row(
            "SELECT sequence, activation_epoch FROM snapshots WHERE actor_id=?1",
            [ACTOR as i64],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(observed, (99, 99));
    assert!(
        !conn.is_autocommit(),
        "fixture transaction unexpectedly committed"
    );
    println!("{CHILD_READY}");
    std::io::stdout().flush().unwrap();
    // The open transaction and connection must not drop before SIGKILL.
    loop {
        std::thread::park();
    }
}

#[cfg(unix)]
#[test]
fn hard_kill_uncommitted_ddl_rolls_back_before_migration_resumes() {
    use std::{
        io::{BufRead, BufReader},
        os::unix::process::ExitStatusExt,
        process::{Child, Command, Stdio},
        sync::mpsc,
        thread,
        time::Duration,
    };

    struct KillChild(Child);
    impl Drop for KillChild {
        fn drop(&mut self) {
            // Cleanup only the process created by this test, including a
            // timeout or panic before the normal kill path.
            if !matches!(self.0.try_wait(), Ok(Some(_))) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
    }

    let nonce = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "nulang-sqlite-interrupted-ddl-{}-{nonce}",
        std::process::id()
    ));
    std::fs::create_dir(&path).unwrap();
    let dir = Disposable(path);
    let db = dir.0.join("interrupted.db");
    let expected = snapshot(0, 3, 7);
    {
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE snapshots (actor_id INTEGER PRIMARY KEY, sequence INTEGER NOT NULL, state TEXT NOT NULL);
             CREATE TABLE journal (actor_id INTEGER NOT NULL, sequence INTEGER NOT NULL, behavior_id INTEGER NOT NULL, payload TEXT NOT NULL, PRIMARY KEY(actor_id, sequence));",
        ).unwrap();
        conn.execute(
            "INSERT INTO snapshots (actor_id, sequence, state) VALUES (?1, 3, ?2)",
            params![
                ACTOR as i64,
                serde_json::to_string(&expected.state).unwrap()
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO journal (actor_id, sequence, behavior_id, payload) VALUES (?1, 4, 2, ?2)",
            params![
                ACTOR as i64,
                serde_json::to_string(&vec![PersistedValue::Int(7)]).unwrap()
            ],
        )
        .unwrap();
    }

    let mut child = KillChild(
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "uncommitted_ddl_child", "--nocapture"])
            .env(CHILD_DIR_ENV, &dir.0)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let stdout = child.0.stdout.take().unwrap();
    let (send, ready) = mpsc::channel();
    let reader = thread::spawn(move || {
        let found = BufReader::new(stdout)
            .lines()
            .map_while(Result::ok)
            .any(|line| line.contains(CHILD_READY));
        let _ = send.send(found);
    });
    assert!(
        ready
            .recv_timeout(Duration::from_secs(30))
            .expect("child readiness deadline"),
        "child exited without readiness marker"
    );
    assert!(
        child.0.try_wait().unwrap().is_none(),
        "writer exited before kill"
    );
    let wal = dir.0.join("interrupted.db-wal");
    assert!(
        std::fs::metadata(&wal).unwrap().len() > 32,
        "transaction did not spill frames to WAL"
    );
    child.0.kill().unwrap();
    assert_eq!(
        child.0.wait().unwrap().signal(),
        Some(9),
        "writer must be SIGKILLed"
    );
    reader.join().unwrap();
    assert!(
        std::fs::metadata(&wal).unwrap().len() > 32,
        "WAL was not retained after kill"
    );

    // The actual production store is the first SQLite opener after the
    // kill. It must roll back the incomplete transaction, add all missing
    // columns and preserve the previously committed snapshot and journal.
    let mut store = LibsqlStore::new(&db).unwrap();
    assert_snapshot(&store, &expected);
    let journal = store.read_journal(ACTOR);
    assert_eq!(journal.len(), 1);
    assert_eq!(journal[0].sequence, 4);
    assert_eq!(journal[0].payload, vec![PersistedValue::Int(7)]);
    let upgraded = snapshot(COLUMNS.len(), 5, 9);
    store.save_snapshot(upgraded.clone()).unwrap();
    drop(store);
    let reopened = LibsqlStore::new(&db).unwrap();
    assert_snapshot(&reopened, &upgraded);
    drop(reopened);
    let conn = Connection::open(&db).unwrap();
    let spill: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE name='uncommitted_spill_fixture'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(spill, 0, "uncommitted DDL leaked through recovery");
    let check: String = conn
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .unwrap();
    assert_eq!(check, "ok");
    println!("SIGKILL during spilled uncommitted DDL: prior state recovered, migration resumed, integrity ok");
}
