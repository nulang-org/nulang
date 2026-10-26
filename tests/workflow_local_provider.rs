//! Local filesystem-backed idempotent provider for deterministic integration
//! testing. Provider acceptance is the local durable record itself; no HTTP.
use nulang::runtime::{
    MemoryStore, PersistedValue, PersistenceStore, WorkflowActivationId, WorkflowEvent,
    WorkflowReplayEventId,
};
use nulang::workflow_local_provider::LocalAcceptanceProvider;
use nulang::workflow_output::read_workflow_output_receipts;
use nulang::workflow_output_delivery::deliver_output_with_deduplication;
use std::{
    fs, io,
    sync::{Arc, Barrier},
    thread,
};

const ACTOR: u64 = 79;
fn fresh_dir() -> std::path::PathBuf {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("nulang-local-provider-{}-{n}", std::process::id()))
}
fn key() -> String {
    "a".repeat(64)
}
fn receipt_store() -> (MemoryStore, nulang::workflow_output::WorkflowOutputReceipt) {
    let mut store = MemoryStore::new();
    store
        .append_workflow_event(
            ACTOR,
            WorkflowEvent::Custom {
                sequence: 1,
                replay_id: Some(WorkflowReplayEventId::new(
                    WorkflowActivationId::new(ACTOR, 12),
                    0,
                )),
                name: "WorkflowOutput".into(),
                args: vec![PersistedValue::String("shipment-ready".into())],
            },
        )
        .unwrap();
    let receipt = read_workflow_output_receipts(&store, ACTOR, 0, 1)
        .unwrap()
        .pop()
        .unwrap();
    (store, receipt)
}

#[test]
fn acceptance_survives_reopen_and_same_key_returns_same_result() {
    let path = fresh_dir();
    let provider = LocalAcceptanceProvider::open(&path).unwrap();
    let first = provider
        .accept(&key(), "local:shipments", b"ready")
        .unwrap();
    assert!(!first.is_empty());
    drop(provider);
    let recovered = LocalAcceptanceProvider::open(&path).unwrap();
    assert_eq!(
        recovered
            .accept(&key(), "local:shipments", b"ready")
            .unwrap(),
        first
    );
    assert_eq!(
        fs::read_dir(&path).unwrap().count(),
        1,
        "one durable accepted operation"
    );
    fs::remove_dir_all(path).unwrap();
}

#[test]
fn rejects_changed_destination_and_payload_under_existing_key() {
    let path = fresh_dir();
    let provider = LocalAcceptanceProvider::open(&path).unwrap();
    provider.accept(&key(), "local:a", b"one").unwrap();
    let changed_dest = provider.accept(&key(), "local:b", b"one").unwrap_err();
    let changed_body = provider.accept(&key(), "local:a", b"two").unwrap_err();
    assert_eq!(changed_dest.kind(), io::ErrorKind::AlreadyExists);
    assert_eq!(changed_body.kind(), io::ErrorKind::AlreadyExists);
    fs::remove_dir_all(path).unwrap();
}

#[test]
fn concurrent_retries_commit_a_single_provider_acceptance() {
    let path = fresh_dir();
    let provider = Arc::new(LocalAcceptanceProvider::open(&path).unwrap());
    let barrier = Arc::new(Barrier::new(8));
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let provider = Arc::clone(&provider);
            let barrier = Arc::clone(&barrier);
            let k = key();
            thread::spawn(move || {
                barrier.wait();
                provider.accept(&k, "local:shipping", b"same")
            })
        })
        .collect();
    let receipts: Vec<_> = threads
        .into_iter()
        .map(|thread| thread.join().unwrap().unwrap())
        .collect();
    assert!(receipts.iter().all(|receipt| receipt == &receipts[0]));
    assert_eq!(fs::read_dir(&path).unwrap().count(), 1);
    fs::remove_dir_all(path).unwrap();
}

#[test]
fn corruption_fails_closed_instead_of_reaccepting_key() {
    let path = fresh_dir();
    let provider = LocalAcceptanceProvider::open(&path).unwrap();
    provider.accept(&key(), "local:shipping", b"same").unwrap();
    fs::write(path.join(format!("{}.json", key())), b"{malformed").unwrap();
    let err = provider
        .accept(&key(), "local:shipping", b"same")
        .unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    assert_eq!(fs::read_dir(&path).unwrap().count(), 1);
    fs::remove_dir_all(path).unwrap();
}

#[test]
fn invalid_key_or_empty_destination_cannot_create_files() {
    let path = fresh_dir();
    let provider = LocalAcceptanceProvider::open(&path).unwrap();
    let uppercase = "A".repeat(64);
    for bad in ["../escape", uppercase.as_str(), "abc"] {
        let err = provider.accept(bad, "local:shipping", b"x").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }
    assert_eq!(
        provider.accept(&key(), " ", b"x").unwrap_err().kind(),
        io::ErrorKind::InvalidInput
    );
    assert_eq!(fs::read_dir(&path).unwrap().count(), 0);
    fs::remove_dir_all(path).unwrap();
}

#[test]
fn actual_receipt_adapter_recovers_lost_response_from_reopened_provider() {
    let path = fresh_dir();
    let provider = LocalAcceptanceProvider::open(&path).unwrap();
    let (mut store, receipt) = receipt_store();
    let first = deliver_output_with_deduplication(
        &mut store,
        ACTOR,
        1,
        &receipt,
        "local:shipping",
        |k, d, p| {
            provider.accept(k, d, p).unwrap();
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "provider accepted but network response lost",
            ))
        },
    );
    assert!(first.is_err());
    let mut restarted_store = store.clone();
    drop(provider);
    let provider = LocalAcceptanceProvider::open(&path).unwrap();
    let recovered = deliver_output_with_deduplication(
        &mut restarted_store,
        ACTOR,
        1,
        &receipt,
        "local:shipping",
        |k, d, p| provider.accept(k, d, p),
    )
    .unwrap();
    let second = deliver_output_with_deduplication(
        &mut restarted_store,
        ACTOR,
        1,
        &receipt,
        "local:shipping",
        |_, _, _| panic!("completed must not dispatch"),
    )
    .unwrap();
    assert_eq!(recovered, second);
    assert_eq!(fs::read_dir(&path).unwrap().count(), 1);
    fs::remove_dir_all(path).unwrap();
}
