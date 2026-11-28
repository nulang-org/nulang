//! Process-boundary regression for CLI durable actor recovery.
//! A fresh process must resume its existing SQLite-independent JSON snapshot
//! instead of silently resetting a durable field to its declared default.
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

struct TempStore(PathBuf);
impl Drop for TempStore {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn persistent_actor_restarts_with_previous_committed_state() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time available")
        .as_nanos();
    let root = TempStore(std::env::temp_dir().join(format!(
        "nulang-durable-cli-{}-{unique}",
        std::process::id()
    )));
    fs::create_dir_all(&root.0).expect("temp directory");
    let source = root.0.join("bank.nula");
    let store = root.0.join("store");
    fs::write(
        &source,
        r#"
        persistent actor BankAccount {
            state durable balance: Int = 0
            behavior deposit(amount: Int) { self.balance = self.balance + amount }
        }
        let acc = spawn BankAccount {} in {
            send acc deposit(7)
            acc
        }
        "#,
    )
    .expect("write program");

    // Each invocation is a completely separate process. In-process tests can
    // accidentally pass because the actor and persistence handle stay alive.
    for expected in [7, 14] {
        let output = Command::new(env!("CARGO_BIN_EXE_nulang"))
            .arg("--store")
            .arg(&store)
            .arg(&source)
            .output()
            .expect("run Nulang CLI");
        assert!(
            output.status.success(),
            "run failed: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

        let snapshots: Vec<_> = fs::read_dir(&store)
            .expect("durable store exists")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path().join("snapshot.json"))
            .filter(|path| path.exists())
            .collect();
        assert_eq!(snapshots.len(), 1, "expected one persistent actor snapshot");
        let bytes = fs::read(&snapshots[0]).expect("read snapshot");
        let snapshot: serde_json::Value = serde_json::from_slice(&bytes).expect("valid snapshot");
        let balance = snapshot["state"]["balance"]["value"]
            .as_i64()
            .expect("durable balance is an integer");
        assert_eq!(
            balance, expected,
            "actor state must survive process restart and then accept a new deposit"
        );
    }
}

#[test]
fn corrupted_snapshot_fails_closed_instead_of_running_with_reset_state() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time available")
        .as_nanos();
    let root = TempStore(std::env::temp_dir().join(format!(
        "nulang-corrupt-cli-{}-{unique}",
        std::process::id()
    )));
    fs::create_dir_all(&root.0).expect("temp directory");
    let source = root.0.join("bank.nula");
    let store = root.0.join("store");
    fs::write(
        &source,
        r#"
        persistent actor BankAccount {
            state durable balance: Int = 0
            behavior deposit(amount: Int) { self.balance = self.balance + amount }
        }
        let acc = spawn BankAccount {} in {
            send acc deposit(7)
            acc
        }
        "#,
    )
    .expect("write program");

    let binary = env!("CARGO_BIN_EXE_nulang");
    let initial = Command::new(binary)
        .arg("--store")
        .arg(&store)
        .arg(&source)
        .output()
        .expect("initial run");
    assert!(
        initial.status.success(),
        "initial run must establish a snapshot"
    );

    let snapshot = fs::read_dir(&store)
        .expect("existing persistent store")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path().join("snapshot.json"))
        .find(|path| path.exists())
        .expect("snapshot");
    fs::write(&snapshot, b"{ invalid snapshot").expect("inject corrupt state");

    let restarted = Command::new(binary)
        .arg("--store")
        .arg(&store)
        .arg(&source)
        .output()
        .expect("restart after corruption");
    assert!(
        !restarted.status.success(),
        "corrupted snapshot must prevent successful CLI execution; stdout={} stderr={}",
        String::from_utf8_lossy(&restarted.stdout),
        String::from_utf8_lossy(&restarted.stderr)
    );
    assert_eq!(
        fs::read(&snapshot).expect("snapshot still present"),
        b"{ invalid snapshot",
        "failed recovery must not overwrite corrupt durable evidence"
    );
}

/// Helper invoked as a separate test process. It checkpoints a real runtime
/// actor, signals that the checkpoint is durable, then stays alive until the
/// parent kills the process with SIGKILL.
#[test]
#[ignore = "internal crash test subprocess only"]
fn crash_checkpoint_helper() {
    use nulang::runtime::{JsonFileStore, PersistedValue, Runtime, StateModel};
    use nulang::vm::Value;
    use std::collections::HashMap;
    use std::io::Write;

    let root = PathBuf::from(
        std::env::var_os("NULANG_CRASH_TEST_STORE").expect("helper requires store path"),
    );
    let mut runtime = Runtime::new();
    runtime.persistence = Box::new(JsonFileStore::new(root).expect("open durable store in child"));
    let mut models = HashMap::new();
    models.insert("balance".to_string(), StateModel::Durable);
    let actor_id = runtime.spawn_persistent_actor(
        Box::new(|| vec![("balance".to_string(), Value::int(0))]),
        models,
    );
    assert_ne!(actor_id, 0);
    runtime
        .actors
        .get_mut(&actor_id)
        .expect("spawned actor exists")
        .set_state_field("balance", Value::int(7));
    runtime.checkpoint_actor(actor_id);
    let snapshot = runtime
        .persistence
        .load_snapshot(actor_id)
        .expect("durable checkpoint committed");
    assert_eq!(snapshot.state.get("balance"), Some(&PersistedValue::Int(7)));
    println!("CRASH_SNAPSHOT_READY");
    std::io::stdout().flush().expect("flush barrier");
    std::thread::park();
}

#[cfg(unix)]
#[test]
fn sigkill_after_durable_checkpoint_recovers_on_fresh_cli_process() {
    use std::io::{BufRead, BufReader};
    use std::process::Stdio;
    use std::sync::mpsc;
    use std::time::Duration;

    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time available")
        .as_nanos();
    let root = TempStore(std::env::temp_dir().join(format!(
        "nulang-kill-recovery-{}-{unique}",
        std::process::id()
    )));
    fs::create_dir_all(&root.0).expect("temporary directory");
    let store = root.0.join("store");

    let mut child = Command::new(std::env::current_exe().expect("current test executable"))
        .arg("--exact")
        .arg("crash_checkpoint_helper")
        .arg("--ignored")
        .arg("--nocapture")
        .env("NULANG_CRASH_TEST_STORE", &store)
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn durable checkpoint process");

    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let stdout = child.stdout.take().expect("child stdout");
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            match line {
                Ok(line) if line.contains("CRASH_SNAPSHOT_READY") => {
                    let _ = ready_tx.send(());
                    return;
                }
                Err(_) => return,
                _ => {}
            }
        }
    });
    if ready_rx.recv_timeout(Duration::from_secs(15)).is_err() {
        let _ = child.kill();
        let _ = child.wait();
        let _ = reader.join();
        panic!("child never reported its durable checkpoint barrier");
    }
    // On Unix, Child::kill sends SIGKILL, not a graceful termination request.
    child.kill().expect("SIGKILL checkpoint process");
    let status = child.wait().expect("reap killed child");
    let _ = reader.join();
    assert!(!status.success(), "child must die abruptly");

    let source = root.0.join("bank.nula");
    fs::write(
        &source,
        r#"
        persistent actor BankAccount {
            state durable balance: Int = 0
            behavior deposit(amount: Int) { self.balance = self.balance + amount }
        }
        let acc = spawn BankAccount {} in {
            send acc deposit(7)
            acc
        }
        "#,
    )
    .expect("write restart program");
    let output = Command::new(env!("CARGO_BIN_EXE_nulang"))
        .arg("--store")
        .arg(&store)
        .arg(&source)
        .output()
        .expect("run fresh CLI after SIGKILL");
    assert!(
        output.status.success(),
        "recovery CLI failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let path = store.join("actor_1").join("snapshot.json");
    let snapshot: serde_json::Value =
        serde_json::from_slice(&fs::read(path).expect("recovered actor snapshot"))
            .expect("parse recovered snapshot");
    assert_eq!(
        snapshot["state"]["balance"]["value"].as_i64(),
        Some(14),
        "new CLI must restore 7 before applying an additional deposit of 7"
    );
}
