#![cfg(all(unix, feature = "durability-fault-injection"))]
//! SIGKILL the real libSQL staged-result transaction before COMMIT, then
//! prove that admission survives and the whole partial completion rolls back.
//! Also kill after result COMMIT, before the host receives acknowledgment.

use nulang::durable_effect::{DurableEffectId, DurableEffectRecord, DurableEffectSpec};
use nulang::durable_effect_persistence::DurableEffectPersistenceRecord;
use nulang::durable_inbox::DurableInboxCommand;
use nulang::lexer::Lexer;
use nulang::parser::Parser;
use nulang::primitives::{DeliverySemantics, EffectBoundary};
use nulang::runtime::{
    ActorSnapshot, DurableOutboxMessage, DurableTransition, JournalEntry, LibsqlStore,
    PersistedValue, PersistenceStore, Runtime, DURABLE_TRANSITION_VERSION,
};
use nulang::typechecker::TypeChecker;
use std::collections::HashMap;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const ACTOR_ID: u64 = 817_234;
const BEHAVIOR: u16 = 0;
const CHILD_DB: &str = "NULANG_STAGED_CRASH_DB";
const CHILD_READY: &str = "NULANG_STAGED_CRASH_READY";
const CHILD_STAGE: &str = "NULANG_STAGED_CRASH_STAGE";

fn request() -> DurableInboxCommand {
    DurableInboxCommand::new(
        "tenant-staged-crash",
        "authenticated-sender",
        "one-logical-command",
        ACTOR_ID,
        BEHAVIOR,
        &[PersistedValue::Int(5)],
    )
    .expect("canonical command ID")
}

fn effect_id() -> DurableEffectId {
    DurableEffectId::derive(ACTOR_ID, "cmd:1", 0, "Audit::Write")
}

fn prepared_effect() -> DurableEffectPersistenceRecord {
    let spec = DurableEffectSpec::new(
        effect_id(),
        "Audit::Write",
        EffectBoundary::External,
        DeliverySemantics::EffectivelyOnceWithDeduplication,
    );
    DurableEffectPersistenceRecord::from_effect(DurableEffectRecord::prepare(spec, b"amount=5"))
}

fn snapshot(sequence: u64, total: i64) -> ActorSnapshot {
    let mut state = HashMap::new();
    state.insert("total".to_owned(), PersistedValue::Int(total));
    ActorSnapshot {
        actor_id: ACTOR_ID,
        activation_epoch: 1,
        sequence,
        schema_name: Some("Counter".to_owned()),
        state,
        ..ActorSnapshot::default()
    }
}

fn admission() -> DurableTransition {
    DurableTransition {
        version: DURABLE_TRANSITION_VERSION,
        actor_id: ACTOR_ID,
        activation_epoch: 1,
        sequence: 1,
        expected_previous_sequence: 0,
        command: Some(JournalEntry {
            sequence: 1,
            behavior_id: BEHAVIOR,
            payload: vec![PersistedValue::Int(5)],
        }),
        snapshot: None,
        workflow_events: vec![],
        domain_events: vec![],
        durable_effects: vec![],
        outbox: vec![],
    }
}

fn completion() -> DurableTransition {
    DurableTransition {
        version: DURABLE_TRANSITION_VERSION,
        actor_id: ACTOR_ID,
        activation_epoch: 1,
        sequence: 2,
        expected_previous_sequence: 1,
        command: None,
        snapshot: Some(snapshot(2, 5)),
        workflow_events: vec![],
        domain_events: vec![],
        durable_effects: vec![prepared_effect()],
        outbox: vec![DurableOutboxMessage {
            destination_actor_id: ACTOR_ID + 1,
            ordinal: 0,
            behavior_id: 0,
            payload: vec![PersistedValue::Int(5)],
        }],
    }
}

fn prepare_database(path: &Path) {
    let mut store = LibsqlStore::new(path).expect("file-backed db");
    store.save_snapshot(snapshot(0, 0)).expect("safe snapshot");
    store.persist_strict_typed_actor_admission(ACTOR_ID).unwrap();
    let accepted = store
        .commit_inbox_transition_detailed(request(), admission())
        .expect("commit original authenticated inbox admission");
    assert!(accepted.newly_accepted);
    assert_eq!(accepted.commit.sequence, 1);
}

fn compile_counter() -> nulang::bytecode::CodeModule {
    let source = r#"
        persistent actor Counter {
            state durable total: Int = 0
            behavior add(by: Int) { self.total = self.total + by }
        }
    "#;
    let tokens = Lexer::new(source).lex().unwrap();
    let ast = Parser::new(tokens).parse_module().unwrap();
    let mut checker = TypeChecker::new();
    checker.check_module(&ast).unwrap();
    let hir = nulang::hir_lower::lower_module(&ast, &checker.inferred_decl_types);
    let mut mir = nulang::mir_lower::lower_module(&hir).unwrap();
    let module = nulang::mir_codegen::compile_mir(&mut mir, "staged_sigkill").unwrap();
    assert_eq!(module.actor_metadata[0].behavior_indices[0] as u16, BEHAVIOR);
    module
}

fn runtime_for_recovery(store: LibsqlStore) -> Runtime {
    let module = compile_counter();
    let offsets = module.behaviors.iter().map(|b| b.code_offset as usize).collect();
    let compensation = module.behaviors.iter().map(|b| b.compensate_offset).collect();
    let mut runtime = Runtime::new();
    runtime.persistence = Box::new(store);
    runtime.register_recovery_module(ACTOR_ID, module, offsets, compensation);
    runtime
}

#[test]
fn staged_completion_crash_worker() {
    let Some(path) = std::env::var_os(CHILD_DB) else {
        return;
    };
    let stage = std::env::var(CHILD_STAGE).expect("parent passes stage");
    let marker = PathBuf::from(std::env::var_os(CHILD_READY).expect("parent passes ready file"));

    if stage == "during-transaction" {
        std::env::set_var(
            "NULANG_INBOX_TEST_HALT_STAGE",
            "after-staged-result-before-tail",
        );
        std::env::set_var("NULANG_INBOX_TEST_HALT_ACTOR", ACTOR_ID.to_string());
        std::env::set_var("NULANG_INBOX_TEST_HALT_READY", &marker);
    }

    let mut store = LibsqlStore::new(PathBuf::from(path)).expect("open prepared WAL db");
    store
        .commit_transition(completion())
        .expect("commit trusted staged completion");
    assert_eq!(stage, "after-commit", "mid-transaction hook must not return");

    // Result committed, but ACK and state-hydration have not been sent.
    std::fs::write(marker, b"completion-committed-no-ack").expect("signal parent");
    loop {
        std::thread::park();
    }
}

struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        if matches!(self.0.try_wait(), Ok(None)) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

struct TempDb {
    path: PathBuf,
}

impl TempDb {
    fn database(&self) -> PathBuf {
        self.path.join("actor.db")
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn kill_at(stage: &str) -> TempDb {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let path = std::env::temp_dir().join(format!(
        "nulang-staged-kill-{}-{stage}-{nanos}",
        std::process::id()
    ));
    std::fs::create_dir(&path).expect("private WAL directory");
    let fixture = TempDb { path };
    prepare_database(&fixture.database());
    let marker = fixture.path.join("ready");

    let mut child = KillOnDrop(
        Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("staged_completion_crash_worker")
            .arg("--nocapture")
            .env(CHILD_DB, fixture.database())
            .env(CHILD_STAGE, stage)
            .env(CHILD_READY, &marker)
            .spawn()
            .expect("spawn independent libSQL writer"),
    );

    let deadline = Instant::now() + Duration::from_secs(25);
    loop {
        if marker.exists() {
            break;
        }
        if let Some(status) = child.0.try_wait().unwrap() {
            panic!("child exited before {stage} crash boundary: {status}");
        }
        assert!(Instant::now() < deadline, "staged crash hook was not reached");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(child.0.try_wait().unwrap().is_none());
    child.0.kill().expect("real SIGKILL");
    assert_eq!(child.0.wait().unwrap().signal(), Some(9));
    fixture
}

fn count(store: &LibsqlStore, table: &str) -> usize {
    store
        .query(
            &format!("SELECT actor_id FROM {table} WHERE actor_id = {ACTOR_ID}"),
            &[],
        )
        .unwrap()
        .len()
}

fn verify_original_admission(store: &LibsqlStore) {
    assert!(store.load_strict_typed_actor_admission(ACTOR_ID).unwrap());
    let receipt = store
        .load_durable_inbox_receipt(request().id())
        .unwrap()
        .expect("original committed receipt");
    assert_eq!(receipt.commit.sequence, 1);
    assert_eq!(store.read_journal(ACTOR_ID).len(), 1);
}

#[test]
fn sigkill_during_staged_completion_rolls_back_snapshot_effects_and_outbox() {
    let fixture = kill_at("during-transaction");
    let mut store = LibsqlStore::new(fixture.database()).unwrap();
    verify_original_admission(&store);
    assert_eq!(store.load_snapshot(ACTOR_ID).unwrap().sequence, 0);
    assert_eq!(store.load_durable_tail_position(ACTOR_ID).unwrap().unwrap().sequence, 1);
    assert!(store.load_durable_effect(ACTOR_ID, effect_id()).unwrap().is_none());
    assert_eq!(count(&store, "durable_effect_records"), 0);
    assert_eq!(count(&store, "durable_outbox"), 0);
    assert_eq!(count(&store, "durable_transitions"), 1);

    let mut quarantined = runtime_for_recovery(store);
    assert_eq!(quarantined.recover_actor(ACTOR_ID), Some(ACTOR_ID));
    assert_eq!(
        quarantined.actors[&ACTOR_ID].mailbox.admitted_command_sequence(),
        Some(1)
    );
    assert_eq!(
        quarantined.actors[&ACTOR_ID]
            .get_state_field("total")
            .and_then(|v| v.as_int()),
        Some(0),
        "partial completion must not leak into live state"
    );

    // The trusted host can re-submit its deterministic completion once.
    let completed = quarantined.persistence.commit_transition(completion()).unwrap();
    assert_eq!(completed.sequence, 2);
    quarantined.finish_admitted_typed_actor_command(ACTOR_ID, 1).unwrap();
    assert_eq!(
        quarantined.actors[&ACTOR_ID]
            .get_state_field("total")
            .and_then(|v| v.as_int()),
        Some(5)
    );
    assert_eq!(
        quarantined.actors[&ACTOR_ID].mailbox.admitted_command_sequence(),
        None
    );
    assert!(quarantined
        .persistence
        .load_durable_effect(ACTOR_ID, effect_id())
        .unwrap()
        .is_some());
    assert_eq!(quarantined.persistence.read_journal(ACTOR_ID).len(), 1);
}

#[test]
fn sigkill_after_staged_commit_before_ack_restores_once_and_suppresses_replay() {
    let fixture = kill_at("after-commit");
    let mut store = LibsqlStore::new(fixture.database()).unwrap();
    verify_original_admission(&store);
    assert_eq!(store.load_snapshot(ACTOR_ID).unwrap().sequence, 2);
    assert_eq!(store.load_durable_tail_position(ACTOR_ID).unwrap().unwrap().sequence, 2);
    assert_eq!(store.read_journal(ACTOR_ID).len(), 1);
    assert_eq!(count(&store, "durable_outbox"), 1);
    assert_eq!(count(&store, "durable_effect_records"), 1);

    let committed = store.commit_transition(completion())
        .expect("retry exact same durable completion after lost ACK");
    assert_eq!(committed.sequence, 2);
    assert_eq!(count(&store, "durable_outbox"), 1);
    assert_eq!(count(&store, "durable_effect_records"), 1);
    assert_eq!(store.read_journal(ACTOR_ID).len(), 1);

    let receipt_retry = store
        .commit_inbox_transition_detailed(request(), admission())
        .expect("retry old inbound receipt after result COMMIT");
    assert!(!receipt_retry.newly_accepted);
    assert_eq!(receipt_retry.commit.sequence, 1);

    let mut runtime = runtime_for_recovery(store);
    assert_eq!(runtime.recover_actor(ACTOR_ID), Some(ACTOR_ID));
    assert!(runtime.actors[&ACTOR_ID].mailbox.requires_strict_durable_admission());
    assert_eq!(runtime.actors[&ACTOR_ID].mailbox.admitted_command_sequence(), None);
    assert_eq!(
        runtime.actors[&ACTOR_ID]
            .get_state_field("total")
            .and_then(|v| v.as_int()),
        Some(5)
    );
}
