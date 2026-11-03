//! Independent OS-process contention gate for the test-only Turso writer.
//!
//! The parent owns a disposable database and two disposable rendezvous files.
//! Each child opens its own Turso connection BEFORE receiving the start signal.
//! The release is simultaneous; at most one competing transition may commit.
//! This is local SQLite multi-process validation, not remote or multi-host proof.
use crate::{rich_probe::rich_transition, turso_writer::commit_transition};
use nulang::runtime::{DurableTransition, LibsqlStore, PersistedValue, PersistenceStore};
use std::{
    error::Error,
    fs, io,
    path::Path,
    process::{Child, Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

const REJECTED_CONFLICT: i32 = 75;
const REJECTED_LOCK: i32 = 76;
const READY_TIMEOUT: Duration = Duration::from_secs(40);
const COMMIT_TIMEOUT: Duration = Duration::from_secs(40);

pub(crate) fn candidate(actor: u64, value: i64) -> DurableTransition {
    let (mut transition, _) = rich_transition(actor);
    transition
        .command
        .as_mut()
        .expect("rich transition must contain command")
        .payload = vec![PersistedValue::Int(value)];
    transition
}

// Child entry: explicitly test-only and requires a unique barrier supplied by
// the parent. This is NOT a general-purpose database write command.
pub(crate) fn writer_child(
    database: &Path,
    actor: u64,
    value: i64,
    barrier: &Path,
) -> Result<(), Box<dyn Error>> {
    if std::env::var("NULANG_TURSO_RACE_TEST_CHILD").as_deref() != Ok("1")
        || !barrier
            .to_string_lossy()
            .contains("nulang-turso-process-race-")
    {
        return Err("process-race child requires an isolated test barrier".into());
    }
    let db_path = database
        .to_str()
        .ok_or("non-UTF8 disposable database filename")?;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async {
        let db = turso::Builder::new_local(db_path).build().await?;
        let connection = db.connect()?;

        // Must signal readiness after opening our connection, but before
        // BEGIN IMMEDIATE, or this would only test sequential retries.
        fs::write(barrier.join(format!("ready-{value}")), b"1")?;
        let deadline = Instant::now() + READY_TIMEOUT;
        while !barrier.join("go").exists() {
            if Instant::now() >= deadline {
                return Err::<(), Box<dyn Error>>("race start signal timed out".into());
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        match commit_transition(&connection, candidate(actor, value)).await {
            Ok(_) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                std::process::exit(REJECTED_CONFLICT);
            }
            Err(error)
                if {
                    let message = error.to_string().to_ascii_lowercase();
                    message.contains("busy") || message.contains("locked")
                } =>
            {
                std::process::exit(REJECTED_LOCK);
            }
            Err(error) => Err(Box::<dyn Error>::from(error)),
        }
    })
}

struct ReapChildren(Vec<Child>);

impl Drop for ReapChildren {
    fn drop(&mut self) {
        for child in &mut self.0 {
            if matches!(child.try_wait(), Ok(None)) {
                let _ = child.kill();
            }
            let _ = child.wait();
        }
    }
}

fn process_statuses(children: &mut ReapChildren) -> Result<Vec<ExitStatus>, Box<dyn Error>> {
    let deadline = Instant::now() + COMMIT_TIMEOUT;
    let mut statuses = vec![None; children.0.len()];
    loop {
        for (index, child) in children.0.iter_mut().enumerate() {
            if statuses[index].is_none() {
                statuses[index] = child.try_wait()?;
            }
        }
        if statuses.iter().all(Option::is_some) {
            return statuses
                .into_iter()
                .map(|status| status.ok_or_else(|| "race child lost its exit status".into()))
                .collect();
        }
        if Instant::now() >= deadline {
            return Err("cross-process durable-commit race timed out".into());
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn prove_committed_records(
    database: &Path,
    actor: u64,
    values: [i64; 2],
) -> Result<(), Box<dyn Error>> {
    // Reopen through Nulang's real production libSQL reader after both
    // independent Turso writer processes have terminated.
    let store = LibsqlStore::new(database)?;
    let tail = store
        .load_durable_tail_position(actor)?
        .ok_or("winning actor has no committed tail")?;
    assert_eq!(
        (tail.activation_epoch, tail.sequence),
        (4, 1),
        "race must commit exactly one initial transition"
    );
    let journal = store.read_journal(actor);
    assert_eq!(journal.len(), 1, "loser must not duplicate command");
    let winner = values
        .into_iter()
        .find(|value| journal[0].payload == vec![PersistedValue::Int(*value)])
        .ok_or("committed command was not written by either contender")?;

    let snapshot = store
        .load_snapshot(actor)
        .ok_or("winner snapshot missing")?;
    assert_eq!(snapshot.sequence, 1);
    assert_eq!(snapshot.activation_epoch, 4);
    assert_eq!(store.read_workflow_events(actor).len(), 2);
    assert_eq!(store.read_events(actor).len(), 2);

    let (winner_transition, latest_effect) = rich_transition(actor);
    let expected_effect = latest_effect.effect().spec().id;
    assert_eq!(
        store.load_durable_effect(actor, expected_effect)?,
        Some(latest_effect),
        "versioned effect recovery must survive process contention"
    );
    let mut canonical_winner = winner_transition;
    canonical_winner.command.as_mut().unwrap().payload = vec![PersistedValue::Int(winner)];
    let expected_digest = canonical_winner
        .digest()?
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();

    for (table, count) in [
        ("snapshots", 1),
        ("journal", 1),
        ("durable_tails", 1),
        ("durable_transitions", 1),
        ("durable_workflow_events", 2),
        ("durable_domain_events", 2),
        ("durable_effect_records", 3),
        ("durable_outbox", 1),
    ] {
        let sql = format!("SELECT COUNT(*) FROM {table} WHERE actor_id={actor}");
        let output = store.query(&sql, &[])?;
        let row: Vec<i64> =
            serde_json::from_str(output.first().ok_or("missing SQL count result")?)?;
        assert_eq!(row, vec![count], "partial or duplicated {table} after race");
    }
    let sql = format!("SELECT digest FROM durable_tails WHERE actor_id={actor}");
    let output = store.query(&sql, &[])?;
    let row: Vec<String> = serde_json::from_str(output.first().ok_or("missing digest result")?)?;
    assert_eq!(
        row,
        vec![expected_digest],
        "committed tail digest must identify exactly the winning child"
    );
    Ok(())
}

pub(crate) fn verify_cross_process_fencing(database: &Path) -> Result<(), Box<dyn Error>> {
    // The caller already created the disposable database through LibsqlStore;
    // no fixture path is supplied externally, and no cloud auth is accessed.
    for (round, actor) in (91_u64..94).enumerate() {
        let barrier = tempfile::Builder::new()
            .prefix("nulang-turso-process-race-")
            .tempdir()?;
        let values = [
            10_000_i64 + (round as i64) * 10,
            20_000_i64 + (round as i64) * 10,
        ];
        let mut children = ReapChildren(Vec::with_capacity(2));
        for value in values {
            let child = Command::new(std::env::current_exe()?)
                .arg("--turso-multiprocess-race-child")
                .arg(database)
                .arg(actor.to_string())
                .arg(value.to_string())
                .arg(barrier.path())
                .env("NULANG_TURSO_RACE_TEST_CHILD", "1")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .spawn()?;
            children.0.push(child);
        }

        let ready_deadline = Instant::now() + READY_TIMEOUT;
        loop {
            if values
                .iter()
                .all(|value| barrier.path().join(format!("ready-{value}")).exists())
            {
                break;
            }
            for child in &mut children.0 {
                if let Some(status) = child.try_wait()? {
                    return Err(format!("race child exited before start signal: {status}").into());
                }
            }
            if Instant::now() >= ready_deadline {
                return Err("not all independent Turso processes became ready".into());
            }
            thread::sleep(Duration::from_millis(10));
        }
        fs::write(barrier.path().join("go"), b"1")?;
        let statuses = process_statuses(&mut children)?;
        let successes = statuses.iter().filter(|status| status.success()).count();
        assert_eq!(
            successes, 1,
            "exactly one conflicting process must commit: {statuses:?}"
        );
        assert!(
            statuses
                .iter()
                .filter(|status| !status.success())
                .all(|status| { matches!(status.code(), Some(REJECTED_CONFLICT | REJECTED_LOCK)) }),
            "losing process must reject conflict or SQLite writer lock: {statuses:?}"
        );
        prove_committed_records(database, actor, values)?;
    }
    println!(
        "PASS: three independent OS-process Turso writer races fence conflicts without torn records"
    );
    Ok(())
}

// Test-first RED: replace the temporary child rejection with the actual
// implementation above when the expected failure is established.
