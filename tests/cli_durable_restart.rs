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
