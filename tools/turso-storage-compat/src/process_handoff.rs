//! Test-only local Turso exclusive file ownership and process handoff proof.
//!
//! Distinct from the strict simultaneous-writers gate: this test establishes
//! the driver's observed fail-closed lock, release on owner exit, and durable
//! epoch fencing after ownership has moved to another OS process.
use crate::{process_race::candidate, turso_writer::commit_transition};
use nulang::runtime::{
    DurableTransition, JournalEntry, LibsqlStore, PersistedValue, PersistenceStore,
    DURABLE_TRANSITION_VERSION,
};
use std::{
    error::Error,
    fs,
    io,
    path::Path,
    process::{Child, Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

const TIMEOUT: Duration = Duration::from_secs(40);
const REJECTED_EXCLUSIVE_FILE_LOCK: i32 = 74;

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if matches!(self.0.try_wait(), Ok(None)) {
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}

fn spawn_child(
    mode: &str,
    database: &Path,
    actor: u64,
    barrier: &Path,
) -> Result<ChildGuard, Box<dyn Error>> {
    let child = Command::new(std::env::current_exe()?)
        .arg(mode)
        .arg(database)
        .arg(actor.to_string())
        .arg(barrier)
        .env("NULANG_TURSO_HANDOFF_TEST_CHILD", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()?;
    Ok(ChildGuard(child))
}

fn wait_exit(child: &mut ChildGuard, label: &str) -> Result<ExitStatus, Box<dyn Error>> {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        if let Some(exit) = child.0.try_wait()? {
            return Ok(exit);
        }
        if Instant::now() >= deadline {
            return Err(format!("{label} process exceeded bounded handoff deadline").into());
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn wait_ready(owner: &mut ChildGuard, barrier: &Path) -> Result<(), Box<dyn Error>> {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        if barrier.join("owner-ready").exists() {
            return Ok(());
        }
        if let Some(exit) = owner.0.try_wait()? {
            return Err(format!("file owner exited before readiness: {exit}").into());
        }
        if Instant::now() >= deadline {
            return Err("file owner never published readiness".into());
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn successor(actor: u64) -> DurableTransition {
    DurableTransition {
        version: DURABLE_TRANSITION_VERSION,
        actor_id: actor,
        activation_epoch: 5,
        sequence: 2,
        expected_previous_sequence: 1,
        command: Some(JournalEntry {
            sequence: 2,
            behavior_id: 11,
            payload: vec![PersistedValue::Int(888)],
        }),
        snapshot: None,
        workflow_events: vec![],
        domain_events: vec![],
        durable_effects: vec![],
        outbox: vec![],
    }
}

fn verify_reopened_state(database: &Path, actor: u64) -> Result<(), Box<dyn Error>> {
    let store = LibsqlStore::new(database)?;
    let tail = store
        .load_durable_tail_position(actor)?
        .ok_or("reopened actor has no durable tail")?;
    assert_eq!(
        (tail.activation_epoch, tail.sequence),
        (5, 2),
        "new owner must have advanced exactly one successor transition"
    );
    let journal = store.read_journal(actor);
    assert_eq!(journal.len(), 2, "handoff must not duplicate committed commands");
    assert_eq!(journal[0].payload, vec![PersistedValue::Int(777)]);
    assert_eq!(journal[1].payload, vec![PersistedValue::Int(888)]);
    assert_eq!(store.read_workflow_events(actor).len(), 2);
    assert_eq!(store.read_events(actor).len(), 2);
    assert_eq!(
        store.load_snapshot(actor).ok_or("initial snapshot missing")?.sequence,
        1,
        "successor with no snapshot must preserve prior complete snapshot"
    );

    let (_, completed) = crate::rich_probe::rich_transition(actor);
    assert_eq!(
        store.load_durable_effect(actor, completed.effect().spec().id)?,
        Some(completed),
        "effect prepared/completed during old owner tenure must recover"
    );
    for (table, expected) in [
        ("snapshots", 1),
        ("journal", 2),
        ("durable_tails", 1),
        ("durable_transitions", 2),
        ("durable_workflow_events", 2),
        ("durable_domain_events", 2),
        ("durable_effect_records", 3),
        ("durable_outbox", 1),
    ] {
        let sql = format!("SELECT COUNT(*) FROM {table} WHERE actor_id={actor}");
        let values = store.query(&sql, &[])?;
        let parsed: Vec<i64> =
            serde_json::from_str(values.first().ok_or("missing handoff table count")?)?;
        assert_eq!(parsed, vec![expected], "{table} changed unexpectedly during handoff");
    }
    let sql = format!("SELECT digest FROM durable_tails WHERE actor_id={actor}");
    let values = store.query(&sql, &[])?;
    let digests: Vec<String> =
        serde_json::from_str(values.first().ok_or("missing handoff digest")?)?;
    let digest = successor(actor)
        .digest()?
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    assert_eq!(digests, vec![digest], "tail digest must match successor");
    Ok(())
}

pub(crate) fn verify_exclusive_file_handoff(database: &Path) -> Result<(), Box<dyn Error>> {
    // All children operate only on the disposable database created by the
    // main test mode via real LibsqlStore schema initialization.
    let barrier = tempfile::Builder::new()
        .prefix("nulang-turso-process-handoff-")
        .tempdir()?;
    let actor = 111;
    let mut owner = spawn_child("--turso-handoff-owner", database, actor, barrier.path())?;
    wait_ready(&mut owner, barrier.path())?;

    let mut contender =
        spawn_child("--turso-handoff-contender", database, actor, barrier.path())?;
    let contested = wait_exit(&mut contender, "exclusive-lock contender")?;
    assert_eq!(
        contested.code(),
        Some(REJECTED_EXCLUSIVE_FILE_LOCK),
        "second process must be denied while the owner holds its database file"
    );
    drop(contender);

    fs::write(barrier.path().join("release"), b"1")?;
    assert!(
        wait_exit(&mut owner, "owner release")?.success(),
        "the initial writer must terminate successfully"
    );
    drop(owner);

    let mut resumed =
        spawn_child("--turso-handoff-successor", database, actor, barrier.path())?;
    assert!(
        wait_exit(&mut resumed, "new owner")?.success(),
        "a fresh process must reopen, fence stale epochs and commit successor"
    );
    drop(resumed);
    verify_reopened_state(database, actor)?;
    println!(
        "PASS: Turso local exclusive file lock, process exit/reacquire, stale epoch fence, and durable handoff"
    );
    Ok(())
}

pub(crate) fn run_handoff_child(
    _mode: &str,
    _database: &Path,
    _actor: u64,
    _barrier: &Path,
) -> Result<(), Box<dyn Error>> {
    // RED: a missing child must fail before parent accepts handoff as proven.
    Err(io::Error::new(io::ErrorKind::Unsupported, "handoff child not implemented").into())
}
