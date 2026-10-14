#![cfg(all(feature = "sqlite", unix))]
//! Actual SIGKILL / disk reopen acceptance matrix for strict typed actor inbox
//! admission. No simulated in-process store clone stands in for a crash.
//!
//! The child is a separate invocation of this test binary, using --exact.
//! The parent kills it only after a ready-file confirms the desired boundary.

use nulang::durable_inbox::DurableInboxCommand;
use nulang::lexer::Lexer;
use nulang::parser::Parser;
use nulang::runtime::{
    ActorSnapshot, DurableTransition, JournalEntry, LibsqlStore, PersistedValue,
    PersistenceStore, Runtime, DURABLE_TRANSITION_VERSION,
};
use nulang::typechecker::TypeChecker;
use std::collections::HashMap;
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const ACTOR_ID: u64 = 91_004;
const EPOCH: u64 = 1;
const BEHAVIOR: u16 = 0;

fn request() -> DurableInboxCommand {
    DurableInboxCommand::new(
        "tenant-one",
        "stable-sender",
        "message-one",
        ACTOR_ID,
        BEHAVIOR,
        &[PersistedValue::Int(5)],
    )
    .expect("stable logical identity")
}

fn snapshot(sequence: u64, value: i64) -> ActorSnapshot {
    let mut state = HashMap::new();
    state.insert("total".to_string(), PersistedValue::Int(value));
    ActorSnapshot {
        actor_id: ACTOR_ID,
        activation_epoch: EPOCH,
        sequence,
        schema_name: Some("Counter".to_string()),
        state,
        ..ActorSnapshot::default()
    }
}

fn transition(sequence: u64, command: bool) -> DurableTransition {
    DurableTransition {
        version: DURABLE_TRANSITION_VERSION,
        actor_id: ACTOR_ID,
        activation_epoch: EPOCH,
        sequence,
        expected_previous_sequence: sequence - 1,
        command: command.then_some(JournalEntry {
            sequence,
            behavior_id: BEHAVIOR,
            payload: vec![PersistedValue::Int(5)],
        }),
        snapshot: (!command).then(|| snapshot(sequence, 5)),
        workflow_events: vec![],
        domain_events: vec![],
        durable_effects: vec![],
        outbox: vec![],
    }
}

fn compile_counter() -> nulang::bytecode::CodeModule {
    let source = r#"
        persistent actor Counter {
            state durable total: Int = 0
            behavior add(by: Int) { self.total = self.total + by }
        }
    "#;
    let tokens = Lexer::new(source).lex().expect("lexer");
    let ast = Parser::new(tokens).parse_module().expect("parser");
    let mut checker = TypeChecker::new();
    checker.check_module(&ast).expect("typechecker");
    let hir = nulang::hir_lower::lower_module(&ast, &checker.inferred_decl_types);
    let mut mir = nulang::mir_lower::lower_module(&hir).expect("MIR lower");
    let module = nulang::mir_codegen::compile_mir(&mut mir, "process_kill_strict")
        .expect("bytecode compiler");
    assert_eq!(module.actor_metadata[0].behavior_indices[0] as u16, BEHAVIOR);
    module
}

fn runtime_for_recovery(store: LibsqlStore) -> Runtime {
    let module = compile_counter();
    let offsets = module
        .behaviors
        .iter()
        .map(|behavior| behavior.code_offset as usize)
        .collect();
    let compensation_offsets = module
        .behaviors
        .iter()
        .map(|behavior| behavior.compensate_offset)
        .collect();
    let mut rt = Runtime::new();
    rt.persistence = Box::new(store);
    rt.register_recovery_module(ACTOR_ID, module, offsets, compensation_offsets);
    rt
}

/// Runs only in a child invoked by the parent acceptance tests below.
/// Intentionally never exits after advertising the durable crash boundary.
#[test]
fn durable_process_kill_worker() {
    let Ok(stage) = std::env::var("NULANG_TEST_CRASH_WINDOW") else {
        return;
    };
    let path = PathBuf::from(std::env::var_os("NULANG_TEST_DB").unwrap());
    let ready = PathBuf::from(std::env::var_os("NULANG_TEST_READY").unwrap());

    let mut store = LibsqlStore::new(&path).expect("open disk-backed store");
    store
        .save_snapshot(snapshot(0, 0))
        .expect("pre-command safe snapshot");
    store
        .persist_strict_typed_actor_admission(ACTOR_ID)
        .expect("persist monotonic strict admission before routing");

    match stage.as_str() {
        "before-admission" => {}
        "after-admission" | "after-completion" => {
            store
                .commit_inbox_transition_detailed(request(), transition(1, true))
                .expect("persist atomic command + inbox receipt");
            if stage == "after-completion" {
                store
                    .commit_transition(transition(2, false))
                    .expect("persist completed actor state");
            }
        }
        _ => panic!("unknown process-kill stage"),
    }

    // The ready file is written only after every required durable commit
    // returned successfully. A real parent SIGKILL ends the process.
    std::fs::write(ready, stage.as_bytes()).expect("publish crash boundary");
    loop {
        std::thread::park();
    }
}

/// Ensure a failed/timeout assertion does not leave the crash child running
/// and holding an open WAL writer on the developer's machine.
struct KillOnDrop(std::process::Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        if matches!(self.0.try_wait(), Ok(None)) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

struct CrashFixture {
    directory: PathBuf,
}

impl CrashFixture {
    fn database(&self) -> PathBuf {
        self.directory.join("actor.db")
    }

    fn reopen(&self) -> LibsqlStore {
        LibsqlStore::new(self.database()).expect("reopen after child SIGKILL")
    }
}

impl Drop for CrashFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

fn kill_at(stage: &str) -> CrashFixture {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("valid clock")
        .as_nanos();
    let directory = std::env::temp_dir().join(format!(
        "nulang-strict-sigkill-{}-{stage}-{unique}",
        std::process::id()
    ));
    std::fs::create_dir(&directory).expect("isolated db directory");
    let fixture = CrashFixture { directory };
    let marker = fixture.directory.join("ready");

    let mut child = KillOnDrop(
        Command::new(std::env::current_exe().expect("test binary path"))
            .arg("--exact")
            .arg("durable_process_kill_worker")
            .arg("--nocapture")
            .env("NULANG_TEST_CRASH_WINDOW", stage)
            .env("NULANG_TEST_DB", fixture.database())
            .env("NULANG_TEST_READY", &marker)
            .spawn()
            .expect("launch separate durable writer"),
    );

    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if marker.exists() {
            break;
        }
        if let Some(status) = child.0.try_wait().expect("poll child") {
            panic!("crash worker exited before the {stage} boundary: {status}");
        }
        assert!(Instant::now() < deadline, "crash worker never reached {stage}");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(child.0.try_wait().unwrap().is_none(), "worker must still be alive");
    child.0.kill().expect("SIGKILL worker at durable boundary");
    let killed = child.0.wait().expect("reap worker");
    assert_eq!(killed.signal(), Some(9), "must use a real SIGKILL");
    fixture
}

#[test]
fn kill_before_admission_leaves_no_receipt_or_command_and_keeps_strict_policy() {
    let fixture = kill_at("before-admission");
    let store = fixture.reopen();
    assert!(store.load_strict_typed_actor_admission(ACTOR_ID).unwrap());
    assert!(store.load_durable_inbox_receipt(request().id()).unwrap().is_none());
    assert!(store.read_journal(ACTOR_ID).is_empty());
    assert!(store.load_durable_tail_position(ACTOR_ID).unwrap().is_none());

    let mut rt = runtime_for_recovery(store);
    assert_eq!(rt.recover_actor(ACTOR_ID), Some(ACTOR_ID));
    assert!(rt.actors[&ACTOR_ID].mailbox.requires_strict_durable_admission());
}

#[test]
fn kill_after_admission_retains_receipt_but_refuses_unsafe_handler_replay() {
    let fixture = kill_at("after-admission");
    let mut store = fixture.reopen();
    let receipt = store
        .load_durable_inbox_receipt(request().id())
        .unwrap()
        .expect("acknowledgment survives SIGKILL");
    assert_eq!(receipt.commit.sequence, 1);
    assert_eq!(store.read_journal(ACTOR_ID).len(), 1);
    assert_eq!(store.load_snapshot(ACTOR_ID).unwrap().sequence, 0);

    let retry = store
        .commit_inbox_transition_detailed(request(), transition(1, true))
        .expect("lost-ACK replay");
    assert!(!retry.newly_accepted);
    assert_eq!(retry.commit, receipt.commit);
    assert_eq!(store.read_journal(ACTOR_ID).len(), 1);

    let mut rt = runtime_for_recovery(store);
    assert_eq!(rt.recover_actor(ACTOR_ID), Some(ACTOR_ID));
    let actor = &rt.actors[&ACTOR_ID];
    assert!(actor.mailbox.requires_strict_durable_admission());
    assert_eq!(actor.mailbox.admitted_command_sequence(), Some(1));
    assert_eq!(actor.get_state_field("total").and_then(|v| v.as_int()), Some(0));
    assert_eq!(
        rt.finish_admitted_typed_actor_command(ACTOR_ID, 1)
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[test]
fn kill_after_completion_recovers_state_and_suppresses_duplicate_command() {
    let fixture = kill_at("after-completion");
    let mut store = fixture.reopen();
    assert!(store.load_strict_typed_actor_admission(ACTOR_ID).unwrap());
    assert_eq!(store.load_durable_tail_position(ACTOR_ID).unwrap().unwrap().sequence, 2);
    assert_eq!(store.load_snapshot(ACTOR_ID).unwrap().sequence, 2);
    assert_eq!(store.read_journal(ACTOR_ID).len(), 1);

    let retry = store
        .commit_inbox_transition_detailed(request(), transition(1, true))
        .expect("idempotent duplicate after completion");
    assert!(!retry.newly_accepted);
    assert_eq!(retry.commit.sequence, 1);
    assert_eq!(store.read_journal(ACTOR_ID).len(), 1);

    let mut rt = runtime_for_recovery(store);
    assert_eq!(rt.recover_actor(ACTOR_ID), Some(ACTOR_ID));
    let actor = &rt.actors[&ACTOR_ID];
    assert!(actor.mailbox.requires_strict_durable_admission());
    assert_eq!(actor.get_state_field("total").and_then(|v| v.as_int()), Some(5));
    assert_eq!(actor.sequence, 2);
}
