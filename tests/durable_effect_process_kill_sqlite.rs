//! Destructive process-kill proof for the durable-effect coordinator + libSQL.
//!
//! The provider here is a small durable idempotency ledger, not a production
//! network provider. The test deliberately kills the process rather than
//! dropping a runtime, and reopens the same SQLite database in a new process.
#![cfg(all(feature = "sqlite", unix))]

use nulang::durable_effect::{DurableEffectId, DurableEffectSpec};
use nulang::durable_effect_runtime::{
    DurableEffectCoordinator, DurableEffectDispatchDecision,
};
use nulang::primitives::{DeliverySemantics, EffectBoundary};
use nulang::runtime::{
    ActorSnapshot, DurableTransition, LibsqlStore, PersistedValue, PersistenceStore,
    DURABLE_TRANSITION_VERSION,
};
use nulang::semantic_identity::{effect_site_id, EffectSiteOwnerKind};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const ACTOR_ID: u64 = 907;
const REQUEST: &[u8] = b"checkout:order-42";
const RESULT: &[u8] = b"provider:charged-order-42";
const CHILD_DIR: &str = "NULANG_KILL_PROOF_DIR";
const CHILD_STAGE: &str = "NULANG_KILL_PROOF_STAGE";

fn operation() -> DurableEffectSpec {
    let site = effect_site_id(
        "durable-kill-proof",
        EffectSiteOwnerKind::Behavior,
        "Checkout.run",
        "Payment.charge",
        0,
    );
    DurableEffectSpec::new(
        DurableEffectId::derive_from_site(ACTOR_ID, "checkout/order-42", site, 0),
        "Payment.charge",
        EffectBoundary::External,
        DeliverySemantics::EffectivelyOnceWithDeduplication,
    )
}

fn paths(label: &str) -> (PathBuf, PathBuf) {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("test clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "nulang-durable-hard-kill-{label}-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir_all(&dir).expect("create isolated test directory");
    let db = dir.join("actor.db");
    (dir, db)
}

fn init_actor_state(store: &mut LibsqlStore) {
    let mut snapshot = ActorSnapshot::default();
    snapshot.actor_id = ACTOR_ID;
    snapshot.sequence = 1;
    snapshot.activation_epoch = 1;
    snapshot.state.insert("balance".into(), PersistedValue::Int(100));
    store
        .commit_transition(DurableTransition {
            version: DURABLE_TRANSITION_VERSION,
            actor_id: ACTOR_ID,
            activation_epoch: 1,
            sequence: 1,
            expected_previous_sequence: 0,
            command: None,
            snapshot: Some(snapshot),
            workflow_events: vec![],
            domain_events: vec![],
            durable_effects: vec![],
            outbox: vec![],
        })
        .expect("persist actor state atomically");
}

/// Models a provider with a persistent idempotency ledger. The operation key
/// is fsynced before the provider reports success; retries with the same key
/// yield exactly the same response without creating a second logical mutation.
fn provider_execute(dir: &Path, id: DurableEffectId) -> Vec<u8> {
    let path = dir.join("provider-committed-key");
    let key = id.idempotency_key();
    if path.exists() {
        assert_eq!(fs::read_to_string(&path).unwrap(), key);
        return RESULT.to_vec();
    }
    let mut file = File::create(&path).expect("create provider mutation");
    file.write_all(key.as_bytes()).expect("persist provider key");
    file.sync_all().expect("fsync provider mutation");
    File::open(dir)
        .expect("open provider directory")
        .sync_all()
        .expect("fsync provider directory");
    RESULT.to_vec()
}

#[test]
fn durable_kill_proof_child() {
    let Ok(dir) = std::env::var(CHILD_DIR) else {
        return; // Ordinary test run must not spawn a long-lived parked child.
    };
    let stage = std::env::var(CHILD_STAGE).expect("child stage");
    let dir = PathBuf::from(dir);
    let mut store = LibsqlStore::new(dir.join("actor.db")).expect("open durable store");
    init_actor_state(&mut store);

    let spec = operation();
    let mut coordinator = DurableEffectCoordinator::new(&mut store, ACTOR_ID, 1);
    assert_eq!(
        coordinator.begin(spec.clone(), REQUEST).unwrap(),
        DurableEffectDispatchDecision::DispatchWithDeduplication {
            operation_id: spec.id
        }
    );

    if stage != "prepared" {
        let receipt = provider_execute(&dir, spec.id);
        if stage == "completed" {
            coordinator.complete(spec.id, REQUEST, receipt).unwrap();
        } else {
            assert_eq!(stage, "provider-committed");
        }
    }

    // Only signal after every operation associated with this stage returned.
    // Parent will hard-kill us while parked, not gracefully shut us down.
    println!("NULANG_KILL_PROOF_READY:{stage}");
    std::io::stdout().flush().unwrap();
    loop {
        std::thread::park();
    }
}

fn kill_child_after(stage: &str, dir: &Path) {
    let mut child = Command::new(std::env::current_exe().expect("current test binary"))
        .arg("--exact")
        .arg("durable_kill_proof_child")
        .arg("--nocapture")
        .env(CHILD_DIR, dir)
        .env(CHILD_STAGE, stage)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn test child");

    let (tx, rx) = mpsc::channel();
    let stdout = child.stdout.take().expect("child stdout");
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            match line {
                Ok(line) if line.contains("NULANG_KILL_PROOF_READY:") => {
                    let _ = tx.send(line);
                    return;
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
    });

    let ready = rx.recv_timeout(Duration::from_secs(45));
    // Kill even on timeout/disconnect to avoid orphaning a test child.
    let kill_result = child.kill();
    let status = child.wait().expect("reap killed child");
    reader.join().expect("join child-output reader");

    assert_eq!(
        ready.expect("child did not acknowledge the durable checkpoint"),
        format!("NULANG_KILL_PROOF_READY:{stage}").to_string()
    );
    kill_result.expect("child must still be alive at the kill point");
    assert!(!status.success(), "child must terminate by hard kill");
}

fn assert_actor_state_unchanged(store: &LibsqlStore) {
    let snapshot = store.load_snapshot(ACTOR_ID).expect("recovered snapshot");
    assert_eq!(snapshot.sequence, 1);
    assert_eq!(snapshot.activation_epoch, 1);
    assert_eq!(
        snapshot.state.get("balance"),
        Some(&PersistedValue::Int(100)),
        "recovery must preserve committed actor state"
    );
}

#[test]
fn hard_kill_after_provider_commit_retries_same_key_without_duplicate_mutation() {
    let (dir, db) = paths("provider-success-receipt-loss");
    kill_child_after("provider-committed", &dir);

    let spec = operation();
    let mut recovered = LibsqlStore::new(&db).unwrap();
    assert_actor_state_unchanged(&recovered);
    assert_eq!(recovered.latest_sequence(ACTOR_ID), 2);
    {
        let mut coordinator = DurableEffectCoordinator::new(&mut recovered, ACTOR_ID, 1);
        assert_eq!(
            coordinator.begin(spec.clone(), REQUEST).unwrap(),
            DurableEffectDispatchDecision::DispatchWithDeduplication {
                operation_id: spec.id
            }
        );
        let deduplicated = provider_execute(&dir, spec.id);
        assert_eq!(deduplicated, RESULT);
        coordinator.complete(spec.id, REQUEST, deduplicated).unwrap();
    }
    assert_eq!(recovered.latest_sequence(ACTOR_ID), 3);
    drop(recovered);

    let mut restarted = LibsqlStore::new(&db).unwrap();
    let mut coordinator = DurableEffectCoordinator::new(&mut restarted, ACTOR_ID, 1);
    assert_eq!(
        coordinator.begin(spec.clone(), REQUEST).unwrap(),
        DurableEffectDispatchDecision::ReplayRecordedResult(RESULT.to_vec())
    );
    assert_eq!(
        fs::read_to_string(dir.join("provider-committed-key")).unwrap(),
        spec.id.idempotency_key()
    );
    assert_eq!(restarted.latest_sequence(ACTOR_ID), 3);
    drop(coordinator);
    drop(restarted);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn hard_kill_after_terminal_receipt_replays_without_provider_redispatch() {
    let (dir, db) = paths("completed-receipt");
    kill_child_after("completed", &dir);

    let spec = operation();
    let mut recovered = LibsqlStore::new(&db).unwrap();
    assert_actor_state_unchanged(&recovered);
    assert_eq!(recovered.latest_sequence(ACTOR_ID), 3);
    assert_eq!(
        DurableEffectCoordinator::new(&mut recovered, ACTOR_ID, 1)
            .begin(spec.clone(), REQUEST)
            .unwrap(),
        DurableEffectDispatchDecision::ReplayRecordedResult(RESULT.to_vec())
    );
    assert_eq!(
        fs::read_to_string(dir.join("provider-committed-key")).unwrap(),
        spec.id.idempotency_key()
    );
    assert_eq!(recovered.latest_sequence(ACTOR_ID), 3);
    drop(recovered);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn hard_kill_after_prepared_intent_replays_without_losing_actor_state() {
    let (dir, db) = paths("prepared-intent");
    kill_child_after("prepared", &dir);

    let spec = operation();
    let mut recovered = LibsqlStore::new(&db).unwrap();
    assert_actor_state_unchanged(&recovered);
    assert_eq!(recovered.latest_sequence(ACTOR_ID), 2);
    assert_eq!(
        DurableEffectCoordinator::new(&mut recovered, ACTOR_ID, 1)
            .begin(spec.clone(), REQUEST)
            .unwrap(),
        DurableEffectDispatchDecision::DispatchWithDeduplication {
            operation_id: spec.id
        }
    );
    assert!(!dir.join("provider-committed-key").exists());
    assert_eq!(recovered.latest_sequence(ACTOR_ID), 2);
    drop(recovered);
    fs::remove_dir_all(dir).unwrap();
}
