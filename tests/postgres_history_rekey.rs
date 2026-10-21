//! PostgreSQL actor-history relocation contracts.
//! These integration tests use the existing NU_TEST_POSTGRES_URL gate; no
//! running database means no external side effects and no false success claim.
#![cfg(feature = "postgres")]

use std::io::ErrorKind;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use nulang::runtime::{
    ActorSnapshot, EventEntry, JournalEntry, PersistedValue, PersistenceStore, PostgresStore,
    WorkflowEvent,
};

fn db() -> Option<PostgresStore> {
    let url = match std::env::var("NU_TEST_POSTGRES_URL") {
        Ok(url) => url,
        Err(_) => {
            assert!(
                std::env::var_os("NU_REQUIRE_POSTGRES_REKEY_TESTS").is_none(),
                "PostgreSQL re-key integration gate requires NU_TEST_POSTGRES_URL"
            );
            return None;
        }
    };
    Some(PostgresStore::new(&url).expect("connect isolated PostgreSQL test instance"))
}

fn ids() -> (u64, u64) {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock")
        .as_micros() as u64;
    // Avoid colliding with rows from earlier test processes on a reused DB.
    let from = stamp * 4 + NEXT.fetch_add(2, Ordering::Relaxed);
    (from, from + 1)
}

fn seed(store: &mut PostgresStore, actor_id: u64) {
    store
        .save_snapshot(ActorSnapshot {
            actor_id,
            sequence: 3,
            state: [("count".to_string(), PersistedValue::Int(42))]
                .into_iter()
                .collect(),
            ..ActorSnapshot::default()
        })
        .unwrap();
    store
        .append_journal(
            actor_id,
            JournalEntry {
                sequence: 3,
                behavior_id: 5,
                payload: vec![PersistedValue::Int(7)],
            },
        )
        .unwrap();
    store
        .append_workflow_event(
            actor_id,
            WorkflowEvent::TimerFired {
                sequence: 3,
                name: "wake".to_string(),
            },
        )
        .unwrap();
    store
        .append_event(
            actor_id,
            EventEntry {
                sequence: 3,
                field_name: "count".to_string(),
                event_name: "Incremented".to_string(),
                args: vec![PersistedValue::Int(7)],
                value: PersistedValue::Int(42),
            },
        )
        .unwrap();
}

#[test]
fn test_postgres_rekey_moves_legacy_history_and_survives_reopen() {
    let Some(mut store) = db() else {
        return;
    };
    let (from, to) = ids();
    seed(&mut store, from);
    store.rekey_actor_history(from, to).unwrap();

    assert!(store.load_snapshot(from).is_none());
    assert!(store.read_journal(from).is_empty());
    assert!(store.read_workflow_events(from).is_empty());
    assert!(store.read_events(from).is_empty());
    drop(store);
    let mut reopened = db().expect("same test connection");
    let snapshot = reopened.load_snapshot(to).expect("new checkpoint");
    assert_eq!(snapshot.actor_id, to);
    assert_eq!(snapshot.state["count"], PersistedValue::Int(42));
    assert_eq!(reopened.read_journal(to)[0].behavior_id, 5);
    assert_eq!(reopened.read_workflow_events(to).len(), 1);
    assert_eq!(reopened.read_events(to)[0].value, PersistedValue::Int(42));
    reopened.clear(to).unwrap();
}

#[test]
fn test_postgres_rekey_refuses_destination_collision_without_partial_mutation() {
    let Some(mut store) = db() else {
        return;
    };
    let (from, to) = ids();
    seed(&mut store, from);
    store
        .append_journal(
            to,
            JournalEntry {
                sequence: 99,
                behavior_id: 9,
                payload: vec![],
            },
        )
        .unwrap();
    let error = store.rekey_actor_history(from, to).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::AlreadyExists);
    assert_eq!(
        store.load_snapshot(from).unwrap().state["count"],
        PersistedValue::Int(42)
    );
    assert_eq!(store.read_journal(from).len(), 1);
    assert_eq!(store.read_events(from).len(), 1);
    assert_eq!(store.read_journal(to)[0].behavior_id, 9);
    store.clear(from).unwrap();
    store.clear(to).unwrap();
}

#[test]
fn test_postgres_rekey_refuses_identity_bound_workflow_history() {
    let Some(mut store) = db() else {
        return;
    };
    let (from, to) = ids();
    seed(&mut store, from);
    store
        .append_workflow_event(
            from,
            WorkflowEvent::StepCompleted {
                sequence: 4,
                activation: Some(nulang::runtime::WorkflowActivationId::new(from, 3)),
                step_name: "done".to_string(),
            },
        )
        .unwrap();
    let error = store.rekey_actor_history(from, to).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unsupported);
    assert!(store.load_snapshot(from).is_some());
    assert!(store.load_snapshot(to).is_none());
    store.clear(from).unwrap();
}

#[test]
fn test_postgres_rekey_refuses_embedded_actor_self_reference() {
    let Some(mut store) = db() else {
        return;
    };
    let (from, to) = ids();
    seed(&mut store, from);
    store
        .append_journal(
            from,
            JournalEntry {
                sequence: 5,
                behavior_id: 8,
                payload: vec![PersistedValue::Actor(from)],
            },
        )
        .unwrap();
    let error = store.rekey_actor_history(from, to).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unsupported);
    assert!(store.load_snapshot(from).is_some());
    assert_eq!(store.read_journal(from).len(), 2);
    store.clear(from).unwrap();
}

#[test]
fn test_postgres_rekey_rejects_missing_source_and_same_identity() {
    let Some(mut store) = db() else {
        return;
    };
    let (from, to) = ids();
    assert_eq!(
        store.rekey_actor_history(from, to).unwrap_err().kind(),
        ErrorKind::NotFound
    );
    seed(&mut store, from);
    assert_eq!(
        store.rekey_actor_history(from, from).unwrap_err().kind(),
        ErrorKind::InvalidInput
    );
    assert!(store.load_snapshot(from).is_some());
    store.clear(from).unwrap();
}

#[test]
fn test_postgres_rekey_refuses_actor_self_reference_in_snapshot() {
    let Some(mut store) = db() else {
        return;
    };
    let (from, to) = ids();
    seed(&mut store, from);
    let mut snapshot = store.load_snapshot(from).unwrap();
    snapshot
        .state
        .insert("self_ref".to_string(), PersistedValue::Actor(from));
    store.save_snapshot(snapshot).unwrap();

    let error = store.rekey_actor_history(from, to).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unsupported);
    assert_eq!(
        store.load_snapshot(from).unwrap().state["self_ref"],
        PersistedValue::Actor(from)
    );
    assert!(store.load_snapshot(to).is_none());
    store.clear(from).unwrap();
}

#[test]
fn test_postgres_rekey_refuses_workflow_replay_identity() {
    let Some(mut store) = db() else {
        return;
    };
    let (from, to) = ids();
    seed(&mut store, from);
    store
        .append_workflow_event(
            from,
            WorkflowEvent::TimerSet {
                sequence: 4,
                replay_id: Some(nulang::runtime::WorkflowReplayEventId::new(
                    nulang::runtime::WorkflowActivationId::new(from, 3),
                    0,
                )),
                name: "timeout".to_string(),
                duration_ms: 100,
            },
        )
        .unwrap();

    let error = store.rekey_actor_history(from, to).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unsupported);
    assert!(store.load_snapshot(from).is_some());
    assert!(store.read_workflow_events(to).is_empty());
    store.clear(from).unwrap();
}


#[test]
fn test_postgres_rekey_survives_abrupt_child_process_exit() {
    if std::env::var_os("NU_POSTGRES_REKEY_CHILD").is_some() {
        let from: u64 = std::env::var("NU_POSTGRES_REKEY_FROM")
            .expect("child from ID")
            .parse()
            .expect("valid child from ID");
        let to: u64 = std::env::var("NU_POSTGRES_REKEY_TO")
            .expect("child to ID")
            .parse()
            .expect("valid child to ID");
        let mut store = db().expect("PostgreSQL test DB in child");
        store
            .rekey_actor_history(from, to)
            .expect("child relocation commits before abrupt exit");

        // Terminate without Rust destructors or closing the client normally.
        // This verifies committed history after process loss, not an
        // interruption in the middle of the transaction.
        #[cfg(unix)]
        unsafe {
            libc::_exit(87);
        }
        #[cfg(not(unix))]
        std::process::exit(87);
    }

    let Some(mut store) = db() else {
        return;
    };
    let (from, to) = ids();
    seed(&mut store, from);
    drop(store);

    let executable = std::env::current_exe().expect("integration test executable");
    let status = std::process::Command::new(executable)
        .args([
            "--exact",
            "test_postgres_rekey_survives_abrupt_child_process_exit",
            "--nocapture",
        ])
        .env("NU_POSTGRES_REKEY_CHILD", "1")
        .env("NU_POSTGRES_REKEY_FROM", from.to_string())
        .env("NU_POSTGRES_REKEY_TO", to.to_string())
        .status()
        .expect("run relocation in isolated child process");
    assert_eq!(
        status.code(),
        Some(87),
        "child must commit relocation and exit without normal cleanup"
    );

    let mut reopened = db().expect("reopen after child process loss");
    assert!(reopened.load_snapshot(from).is_none());
    let snapshot = reopened.load_snapshot(to).expect("relocated checkpoint");
    assert_eq!(snapshot.state["count"], PersistedValue::Int(42));
    assert_eq!(reopened.read_journal(to).len(), 1);
    assert_eq!(reopened.read_workflow_events(to).len(), 1);
    assert_eq!(reopened.read_events(to).len(), 1);
    reopened.clear(to).unwrap();
}
