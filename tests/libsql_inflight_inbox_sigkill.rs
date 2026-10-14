#![cfg(all(unix, feature = "durability-fault-injection"))]
//! SIGKILL the *production libSQL commit path* while its IMMEDIATE
//! transaction is open, after snapshot and command writes but before receipt.
//! The injection hook is compiled only with an explicit test feature.

use nulang::durable_inbox::DurableInboxCommand;
use nulang::runtime::{
    ActorSnapshot, DurableOutboxMessage, DurableTransition, JournalEntry, LibsqlStore,
    PersistedValue, PersistenceStore, DURABLE_TRANSITION_VERSION,
};
use std::collections::HashMap;
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const ACTOR_ID: u64 = 817_233;
const CHILD_ENV: &str = "NULANG_INFLIGHT_TEST_DB";
const READY_ENV: &str = "NULANG_INFLIGHT_TEST_READY";

fn request() -> DurableInboxCommand {
    DurableInboxCommand::new(
        "tenant-inflight",
        "verified-transport-sender",
        "logical-message-1",
        ACTOR_ID,
        12,
        &[PersistedValue::Int(5)],
    )
    .expect("stable command ID")
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

fn transition() -> DurableTransition {
    DurableTransition {
        version: DURABLE_TRANSITION_VERSION,
        actor_id: ACTOR_ID,
        activation_epoch: 1,
        sequence: 1,
        expected_previous_sequence: 0,
        command: Some(JournalEntry {
            sequence: 1,
            behavior_id: 12,
            payload: vec![PersistedValue::Int(5)],
        }),
        snapshot: Some(snapshot(1, 5)),
        workflow_events: vec![],
        domain_events: vec![],
        durable_effects: vec![],
        outbox: vec![DurableOutboxMessage {
            destination_actor_id: ACTOR_ID + 1,
            ordinal: 0,
            behavior_id: 2,
            payload: vec![PersistedValue::Int(9)],
        }],
    }
}

/// Only launched as a subprocess by the parent test. If the production
/// transaction hook is not reached, this function returns and parent fails.
#[test]
fn crash_mid_transaction_worker() {
    let Some(db) = std::env::var_os(CHILD_ENV) else {
        return;
    };
    let ready = std::env::var_os(READY_ENV).expect("parent must provide ready marker");

    // The fault injection is deliberately enabled only for this child and
    // bound to this exact actor. It blocks between journal INSERT and COMMIT.
    std::env::set_var(
        "NULANG_INBOX_TEST_HALT_STAGE",
        "after-journal-before-receipt",
    );
    std::env::set_var("NULANG_INBOX_TEST_HALT_ACTOR", ACTOR_ID.to_string());
    std::env::set_var("NULANG_INBOX_TEST_HALT_READY", ready);

    let mut store = LibsqlStore::new(PathBuf::from(db)).expect("reopen prepared local db");
    let _never_returns = store
        .commit_inbox_transition(request(), transition())
        .expect("fault hook must park before the commit returns");
    panic!("unsafe: transaction unexpectedly committed without fault hook");
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

#[test]
fn sigkill_inside_open_inbox_transaction_rolls_back_every_record() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "nulang-inflight-inbox-{}-{unique}",
        std::process::id()
    ));
    std::fs::create_dir(&dir).expect("isolated database directory");
    struct RemoveDir(PathBuf);
    impl Drop for RemoveDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = RemoveDir(dir.clone());
    let db = dir.join("actor.db");
    let marker = dir.join("transaction-open");

    {
        let mut initial = LibsqlStore::new(&db).expect("prepare WAL database");
        initial.save_snapshot(snapshot(0, 0)).expect("safe baseline");
        initial
            .persist_strict_typed_actor_admission(ACTOR_ID)
            .expect("strict marker must survive");
    }

    let mut child = KillOnDrop(
        Command::new(std::env::current_exe().expect("test executable"))
            .arg("--exact")
            .arg("crash_mid_transaction_worker")
            .arg("--nocapture")
            .env(CHILD_ENV, &db)
            .env(READY_ENV, &marker)
            .spawn()
            .expect("launch crash-injected transaction"),
    );
    let deadline = Instant::now() + Duration::from_secs(25);
    loop {
        if marker.exists() {
            break;
        }
        if let Some(status) = child.0.try_wait().expect("poll worker") {
            panic!("worker exited before journal-write fault boundary: {status}");
        }
        assert!(
            Instant::now() < deadline,
            "worker never reached in-flight transaction fault boundary"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        child.0.try_wait().unwrap().is_none(),
        "child must still hold the uncommitted transaction"
    );
    child.0.kill().expect("send SIGKILL during open SQL transaction");
    assert_eq!(
        child.0.wait().expect("reap child").signal(),
        Some(9),
        "must terminate an actual child with SIGKILL"
    );

    let mut recovered = LibsqlStore::new(&db).expect("reopen WAL after SIGKILL");
    assert!(
        recovered.load_strict_typed_actor_admission(ACTOR_ID).unwrap(),
        "previously committed strict policy must survive"
    );
    let previous = recovered.load_snapshot(ACTOR_ID).expect("safe snapshot");
    assert_eq!(previous.sequence, 0);
    assert_eq!(previous.state.get("total"), Some(&PersistedValue::Int(0)));
    assert!(recovered.read_journal(ACTOR_ID).is_empty());
    assert!(recovered.load_durable_tail_position(ACTOR_ID).unwrap().is_none());
    assert!(recovered
        .load_durable_inbox_receipt(request().id())
        .unwrap()
        .is_none());
    for table in [
        "durable_transitions",
        "durable_effect_records",
        "durable_outbox",
        "durable_domain_events",
        "durable_workflow_events",
    ] {
        let rows = recovered
            .query(
                &format!("SELECT actor_id FROM {table} WHERE actor_id = {ACTOR_ID}"),
                &[],
            )
            .expect("query surviving durable records");
        assert!(rows.is_empty(), "uncommitted {table} leaked across SIGKILL");
    }

    // After rollback, the exact same logical key must be *new*, not an
    // incorrectly persisted duplicate or a half-committed transition.
    let accepted = recovered
        .commit_inbox_transition_detailed(request(), transition())
        .expect("retry previously uncommitted command");
    assert!(accepted.newly_accepted);
    assert_eq!(accepted.commit.sequence, 1);
    assert_eq!(recovered.read_journal(ACTOR_ID).len(), 1);
    assert_eq!(recovered.load_snapshot(ACTOR_ID).unwrap().sequence, 1);
    assert!(recovered
        .load_durable_inbox_receipt(request().id())
        .unwrap()
        .is_some());
    assert_eq!(
        recovered
            .query(
                &format!("SELECT actor_id FROM durable_outbox WHERE actor_id = {ACTOR_ID}"),
                &[],
            )
            .unwrap()
            .len(),
        1
    );
}
