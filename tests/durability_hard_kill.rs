use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use nulang::runtime::{
    ActorSnapshot, DurableTransition, JsonFileStore, PersistedValue, PersistenceStore,
    WorkflowEvent, DURABLE_TRANSITION_VERSION,
};

const ACTOR_ID: u64 = 29;
const CHILD_ENV: &str = "NU_DURABILITY_HARD_KILL_CHILD";
const STORE_ENV: &str = "NU_DURABILITY_HARD_KILL_STORE";

fn transition(sequence: u64, expected_previous_sequence: u64) -> DurableTransition {
    let mut state = HashMap::new();
    state.insert(
        "acknowledged_sequence".to_string(),
        PersistedValue::Int(sequence as i64),
    );

    DurableTransition {
        version: DURABLE_TRANSITION_VERSION,
        actor_id: ACTOR_ID,
        activation_epoch: 1,
        sequence,
        expected_previous_sequence,
        command: None,
        snapshot: Some(ActorSnapshot {
            actor_id: ACTOR_ID,
            sequence,
            state,
            waiting_signal: None,
            crdt_snapshot: None,
            crdt_field_map: None,
            authority_tokens: Default::default(),
        }),
        workflow_events: vec![WorkflowEvent::Custom {
            sequence,
            name: "acknowledged".to_string(),
            args: vec![PersistedValue::Int(sequence as i64)],
        }],
        domain_events: Vec::new(),
        durable_effects: Vec::new(),
        outbox: Vec::new(),
    }
}

fn child_writer() {
    let store_dir = std::env::var_os(STORE_ENV).expect("child store path must be provided");
    let mut store = JsonFileStore::new(store_dir).expect("child must open JSON durable store");

    let commit = store
        .commit_transition(transition(1, 0))
        .expect("acknowledged transition must commit");

    assert_eq!(commit.sequence, 1);
    println!("NU_DURABILITY_ACK {}", commit.sequence);
    std::io::stdout()
        .flush()
        .expect("child must flush acknowledgement");

    // The parent deliberately terminates us after observing the ACK. Staying
    // alive here ensures a successful test always exercises abrupt process
    // termination rather than a normal child exit.
    loop {
        std::thread::park();
    }
}

fn fresh_dir() -> std::path::PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock must be after UNIX epoch")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "nulang-hard-kill-{}-{unique}",
        std::process::id()
    ))
}

#[test]
fn acknowledged_json_atomic_transition_survives_immediate_hard_kill() {
    if std::env::var_os(CHILD_ENV).is_some() {
        child_writer();
        return;
    }

    let store_dir = fresh_dir();
    let current_test_binary = std::env::current_exe().expect("test binary path must be available");

    let mut child = Command::new(current_test_binary)
        .arg("--exact")
        .arg("acknowledged_json_atomic_transition_survives_immediate_hard_kill")
        .arg("--nocapture")
        .env(CHILD_ENV, "1")
        .env(STORE_ENV, &store_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("durability child process must start");

    let stdout = child
        .stdout
        .take()
        .expect("durability child stdout must be piped");
    let mut reader = BufReader::new(stdout);
    let mut acknowledged_sequence = None;
    let mut line = String::new();

    loop {
        line.clear();
        let bytes = reader
            .read_line(&mut line)
            .expect("parent must read child acknowledgement");
        if bytes == 0 {
            break;
        }
        if let Some(value) = line.trim().strip_prefix("NU_DURABILITY_ACK ") {
            acknowledged_sequence = Some(
                value
                    .parse::<u64>()
                    .expect("child acknowledgement must contain a sequence"),
            );
            break;
        }
    }

    assert_eq!(
        acknowledged_sequence,
        Some(1),
        "child exited or stopped producing output before acknowledging the durable commit"
    );

    // Child::kill is an abrupt termination primitive (SIGKILL on Unix and
    // TerminateProcess on Windows). The child is intentionally parked above,
    // so this cannot be confused with graceful shutdown.
    child
        .kill()
        .expect("parent must be able to hard-kill durability child");
    let status = child.wait().expect("parent must reap durability child");
    assert!(
        !status.success(),
        "durability child must terminate abruptly rather than exit normally"
    );

    let reopened = JsonFileStore::new(&store_dir).expect("parent must reopen JSON durable store");

    let tail = reopened
        .load_durable_tail(ACTOR_ID)
        .expect("durable tail recovery must not fail")
        .expect("acknowledged durable tail must survive hard kill");
    assert_eq!(tail.activation_epoch, 1);
    assert_eq!(tail.sequence, 1);
    assert_eq!(reopened.latest_sequence(ACTOR_ID), 1);

    let snapshot = reopened
        .load_snapshot(ACTOR_ID)
        .expect("acknowledged snapshot must survive hard kill");
    assert_eq!(snapshot.sequence, 1);
    assert_eq!(
        snapshot.state.get("acknowledged_sequence"),
        Some(&PersistedValue::Int(1))
    );

    let events = reopened.read_workflow_events(ACTOR_ID);
    assert_eq!(events.len(), 1);
    assert!(matches!(
        &events[0],
        WorkflowEvent::Custom {
            sequence: 1,
            name,
            args
        } if name == "acknowledged" && args == &vec![PersistedValue::Int(1)]
    ));

    std::fs::remove_dir_all(&store_dir).expect("test store cleanup must succeed");
}
