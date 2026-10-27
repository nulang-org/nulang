//! Executable-only cross-process Turso acceptance tests.
//! No production runtime imports this module and no real database is touched.
use crate::turso_local_commit;
use nulang::durable_effect::{DurableEffectId, DurableEffectRecord, DurableEffectSpec};
use nulang::durable_effect_persistence::DurableEffectPersistenceRecord;
use nulang::primitives::{DeliverySemantics, EffectBoundary};
use nulang::runtime::{
    ActorSnapshot, DurableOutboxMessage, DurableTransition, EventEntry, JournalEntry, LibsqlStore,
    PersistedValue, PersistenceStore, WorkflowActivationId, WorkflowEvent,
    DURABLE_TRANSITION_VERSION,
};
use nulang::semantic_identity::{effect_site_id, EffectSiteOwnerKind};
use std::{
    collections::HashMap,
    error::Error,
    fs, io,
    path::Path,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

fn transition(actor_id: u64, epoch: u64, payload: i64) -> DurableTransition {
    let effect_site = effect_site_id(
        "nulang-cross-process-turso",
        EffectSiteOwnerKind::Behavior,
        "RecoveryProbe.run",
        "Provider.ask",
        0,
    );
    let effect_id = DurableEffectId::derive_from_site(actor_id, "turn:1", effect_site, 0);
    let prepared = DurableEffectPersistenceRecord::from_effect(DurableEffectRecord::prepare(
        DurableEffectSpec::new(
            effect_id,
            "Provider.ask",
            EffectBoundary::External,
            DeliverySemantics::AtLeastOnce,
        ),
        b"cross-process-effect",
    ));
    let mut state = HashMap::new();
    state.insert("payload".to_string(), PersistedValue::Int(payload));
    DurableTransition {
        version: DURABLE_TRANSITION_VERSION,
        actor_id,
        activation_epoch: epoch,
        sequence: 1,
        expected_previous_sequence: 0,
        command: Some(JournalEntry {
            sequence: 1,
            behavior_id: 42,
            payload: vec![PersistedValue::Int(payload)],
        }),
        snapshot: Some(ActorSnapshot {
            actor_id,
            sequence: 1,
            activation_epoch: epoch,
            state,
            ..ActorSnapshot::default()
        }),
        workflow_events: vec![WorkflowEvent::StepCompleted {
            sequence: 1,
            activation: Some(WorkflowActivationId::new(actor_id, 1)),
            step_name: format!("payload-{payload}"),
        }],
        domain_events: vec![EventEntry {
            sequence: 1,
            field_name: "payload".to_string(),
            event_name: "Written".to_string(),
            args: vec![],
            value: PersistedValue::Int(payload),
        }],
        durable_effects: vec![prepared],
        outbox: vec![DurableOutboxMessage {
            destination_actor_id: actor_id + 1,
            ordinal: 0,
            behavior_id: 43,
            payload: vec![PersistedValue::Int(payload)],
        }],
    }
}

fn parse_arg<T: std::str::FromStr>(value: &str) -> Result<T, Box<dyn Error>>
where
    <T as std::str::FromStr>::Err: Error + 'static,
{
    Ok(value.parse::<T>()?)
}

fn await_file(path: &Path, timeout: Duration) -> io::Result<()> {
    let deadline = Instant::now() + timeout;
    while !path.exists() {
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("worker barrier timed out: {}", path.display()),
            ));
        }
        thread::sleep(Duration::from_millis(5));
    }
    Ok(())
}

/// Called only when the manual binary is explicitly invoked with the
/// private --process-worker mode. Never exposed from Nulang production CLI.
pub fn worker(args: &[String]) -> Result<(), Box<dyn Error>> {
    if args.len() != 7 {
        return Err("expected worker mode/db/actor/epoch/payload/ready/go".into());
    }
    let mode = args[0].as_str();
    if !matches!(mode, "race" | "direct" | "precommit-kill") {
        return Err("unknown test worker mode".into());
    }
    let db = &args[1];
    let actor: u64 = parse_arg(&args[2])?;
    let epoch: u64 = parse_arg(&args[3])?;
    let payload: i64 = parse_arg(&args[4])?;
    let ready = Path::new(&args[5]);
    let go = Path::new(&args[6]);

    if mode == "race" {
        fs::write(ready, b"ready")?;
        await_file(go, Duration::from_secs(30))?;
    }
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async {
        let db = turso::Builder::new_local(db).build().await?;
        let conn = db.connect()?;
        let t = transition(actor, epoch, payload);
        if mode == "precommit-kill" {
            turso_local_commit::commit_with_precommit_hook(&conn, &t, || {
                let mut marker = fs::File::create(ready).expect("create post-write marker");
                use std::io::Write;
                marker
                    .write_all(b"all-records-staged")
                    .expect("write marker");
                marker.sync_all().expect("flush marker");
                // Parent must SIGKILL this process before it can COMMIT.
                loop {
                    thread::park();
                }
            })
            .await?;
        } else {
            turso_local_commit::commit(&conn, &t).await?;
        }
        println!("COMMITTED:{actor}:{payload}");
        Ok::<(), Box<dyn Error>>(())
    })
}

struct WorkerSpec<'a> {
    mode: &'a str,
    db: &'a Path,
    actor: u64,
    epoch: u64,
    payload: i64,
    ready: &'a Path,
    go: &'a Path,
}

fn spawn(exe: &Path, spec: WorkerSpec<'_>) -> io::Result<Child> {
    Command::new(exe)
        .arg("--process-worker")
        .arg(spec.mode)
        .arg(spec.db)
        .arg(spec.actor.to_string())
        .arg(spec.epoch.to_string())
        .arg(spec.payload.to_string())
        .arg(spec.ready)
        .arg(spec.go)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
}

fn wait_bounded(child: &mut Child, timeout: Duration) -> io::Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if child.try_wait()?.is_some() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "child writer timed out",
            ));
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn count(store: &LibsqlStore, table: &str, actor: u64) -> Result<i64, Box<dyn Error>> {
    // Only known statically hardcoded schema table names are allowed.
    assert!(matches!(
        table,
        "durable_tails"
            | "durable_transitions"
            | "snapshots"
            | "journal"
            | "durable_workflow_events"
            | "durable_domain_events"
            | "durable_effect_records"
            | "durable_outbox"
    ));
    let sql = format!("SELECT COUNT(*) FROM {table} WHERE actor_id={actor}");
    let json = store.query(&sql, &[])?;
    let row: Vec<i64> = serde_json::from_str(&json[0])?;
    Ok(row[0])
}

fn assert_atomic_rows(
    store: &LibsqlStore,
    actor: u64,
    expected: i64,
) -> Result<(), Box<dyn Error>> {
    for table in [
        "durable_tails",
        "durable_transitions",
        "snapshots",
        "journal",
        "durable_workflow_events",
        "durable_domain_events",
        "durable_effect_records",
        "durable_outbox",
    ] {
        assert_eq!(
            count(store, table, actor)?,
            expected,
            "partial or duplicate {table} for actor {actor}"
        );
    }
    Ok(())
}

fn competition(exe: &Path, root: &Path, case: usize) -> Result<(), Box<dyn Error>> {
    let db = root.join(format!("race-{case}.db"));
    {
        // Only use the production schema bootstrap. Close its connection
        // before unrelated child processes try to write.
        let _ = LibsqlStore::new(&db)?;
    }
    let go = root.join(format!("race-{case}-go"));
    let one_ready = root.join(format!("race-{case}-a-ready"));
    let two_ready = root.join(format!("race-{case}-b-ready"));
    let actor = 501_u64 + case as u64;
    let mut one = spawn(
        exe,
        WorkerSpec {
            mode: "race",
            db: &db,
            actor,
            epoch: 3,
            payload: 1001,
            ready: &one_ready,
            go: &go,
        },
    )?;
    let mut two = spawn(
        exe,
        WorkerSpec {
            mode: "race",
            db: &db,
            actor,
            epoch: 3,
            payload: 2002,
            ready: &two_ready,
            go: &go,
        },
    )?;
    let setup = await_file(&one_ready, Duration::from_secs(15))
        .and_then(|_| await_file(&two_ready, Duration::from_secs(15)));
    if let Err(error) = setup {
        let _ = one.kill();
        let _ = two.kill();
        let _ = one.wait();
        let _ = two.wait();
        return Err(error.into());
    }
    fs::write(&go, b"go")?;
    wait_bounded(&mut one, Duration::from_secs(30))?;
    wait_bounded(&mut two, Duration::from_secs(30))?;
    let one_output = one.wait_with_output()?;
    let two_output = two.wait_with_output()?;
    let winning_payload = match (one_output.status.success(), two_output.status.success()) {
        (true, false) => 1001,
        (false, true) => 2002,
        _ => {
            return Err(format!(
                "expected one writer to win, not both or neither: first={:?}/{} second={:?}/{}",
                one_output.status,
                String::from_utf8_lossy(&one_output.stderr),
                two_output.status,
                String::from_utf8_lossy(&two_output.stderr),
            )
            .into());
        }
    };

    let store = LibsqlStore::new(&db)?;
    assert_atomic_rows(&store, actor, 1)?;
    assert_eq!(
        store
            .load_durable_tail_position(actor)?
            .ok_or("race tail missing")?
            .sequence,
        1
    );
    let expected = transition(actor, 3, winning_payload);
    assert_eq!(
        store
            .load_snapshot(actor)
            .ok_or("race snapshot missing")?
            .state
            .get("payload"),
        Some(&PersistedValue::Int(winning_payload))
    );
    let recorded = store.query(
        &format!("SELECT digest FROM durable_tails WHERE actor_id={actor}"),
        &[],
    )?;
    let row: Vec<String> = serde_json::from_str(&recorded[0])?;
    let digest = expected.digest()?;
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(row[0], hex, "winner's digest differs from atomic tail");
    println!(
        "PASS cross-process conflicting writers case {case}, winner payload {winning_payload}"
    );
    Ok(())
}

fn crash_before_commit(exe: &Path, root: &Path) -> Result<(), Box<dyn Error>> {
    let db = root.join("precommit-crash.db");
    {
        let _ = LibsqlStore::new(&db)?;
    }
    let actor = 900_u64;
    let ready = root.join("precommit-staged");
    let unused = root.join("unused-go");
    let mut child = spawn(
        exe,
        WorkerSpec {
            mode: "precommit-kill",
            db: &db,
            actor,
            epoch: 7,
            payload: 7070,
            ready: &ready,
            go: &unused,
        },
    )?;
    if let Err(e) = await_file(&ready, Duration::from_secs(25)) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(e.into());
    }
    child.kill()?;
    let status = child.wait()?;
    assert!(!status.success(), "pre-commit child unexpectedly completed");
    {
        let store = LibsqlStore::new(&db)?;
        assert_atomic_rows(&store, actor, 0)?;
        assert!(
            store.load_durable_tail_position(actor)?.is_none(),
            "uncommitted tail survived a killed child"
        );
    }
    // A second independent child must be able to acquire the freed SQLite
    // writer lock, complete the same logical command, and recover normally.
    let mut retry = spawn(
        exe,
        WorkerSpec {
            mode: "direct",
            db: &db,
            actor,
            epoch: 7,
            payload: 7070,
            ready: &unused,
            go: &unused,
        },
    )?;
    wait_bounded(&mut retry, Duration::from_secs(30))?;
    let result = retry.wait_with_output()?;
    if !result.status.success() {
        return Err(format!(
            "post-crash retry failed: {}",
            String::from_utf8_lossy(&result.stderr)
        )
        .into());
    }
    let store = LibsqlStore::new(&db)?;
    assert_atomic_rows(&store, actor, 1)?;
    assert_eq!(
        store
            .load_durable_tail_position(actor)?
            .ok_or("retry tail missing")?
            .sequence,
        1
    );
    println!("PASS SIGKILL before COMMIT: complete rollback and clean child retry");
    Ok(())
}

pub fn verify() -> Result<(), Box<dyn Error>> {
    let root = tempfile::tempdir()?;
    let exe = std::env::current_exe()?;
    for n in 0..12 {
        competition(&exe, root.path(), n)?;
    }
    crash_before_commit(&exe, root.path())?;
    Ok(())
}
