//! Atomic epoch/sequence fencing for verified pinned workflow recovery.
//! Uses real persistence implementations for the ownership race boundary.

use nulang::artifact_identity::ArtifactIdentityManifest;
use nulang::behavior_manifest::{
    BehaviorArtifact, BehaviorManifest, BehaviorPackage, BEHAVIOR_ARTIFACT_KIND_NBC_V1,
    BEHAVIOR_MANIFEST_SCHEMA,
};
use nulang::content_identity::SemanticId;
use nulang::runtime::{
    claim_pinned_workflow_recovery, commit_pinned_workflow_start,
    verify_pinned_workflow_resume, ActorSnapshot, DurableTailPosition,
    DurableTransition, MemoryStore, PersistenceStore, DURABLE_TRANSITION_VERSION,
};
use std::io::ErrorKind;

fn behavior(bytes: &[u8]) -> BehaviorManifest {
    let identity = ArtifactIdentityManifest::new(
        None, SemanticId::from_canonical_bytes(b"fenced-resume", []),
        "compiler-v1", "bytecode", "abi-v1", "bytecode",
        std::iter::empty::<&str>(),
    );
    BehaviorManifest {
        schema: BEHAVIOR_MANIFEST_SCHEMA.into(),
        package: BehaviorPackage {
            name: "payments".into(),
            version: "1".into(),
            language_version: "1".into(),
        },
        artifact: BehaviorArtifact {
            kind: BEHAVIOR_ARTIFACT_KIND_NBC_V1.into(),
            digest: format!("blake3:{}", blake3::hash(bytes).to_hex()),
            source_id: None,
            semantic_id: identity.semantic_id().to_string(),
            artifact_id: identity.artifact_id().to_string(),
            compiler_version: identity.compiler_version().into(),
            target: identity.target().into(),
            abi: identity.abi().into(),
            backend: identity.backend().into(),
            flags: vec![],
        },
        actors: vec![],
    }
}

fn start(store: &mut dyn PersistenceStore, id: u64) {
    let bytes = b"fenced-workflow-v1";
    commit_pinned_workflow_start(
        store, id, "payments/one", "Payment", &behavior(bytes), bytes,
        ActorSnapshot {
            actor_id: id,
            sequence: 1,
            activation_epoch: 1,
            ..ActorSnapshot::default()
        },
        vec![],
    ).unwrap();
}

#[test]
fn pinned_recovery_claim_atomically_advances_sequence_and_epoch() {
    let mut store = MemoryStore::new();
    start(&mut store, 42);
    let observed = store.load_durable_tail_position(42).unwrap().unwrap();
    let claim = claim_pinned_workflow_recovery(&mut store, 42, observed).unwrap();
    assert_eq!(claim.sequence, 2);
    assert_eq!(claim.activation_epoch, 2);
    let snapshot = store.load_snapshot(42).unwrap();
    assert_eq!(snapshot.sequence, 2);
    assert_eq!(snapshot.activation_epoch, 2);
    verify_pinned_workflow_resume(
        &store, 42, "payments/one", &behavior(b"fenced-workflow-v1"),
        b"fenced-workflow-v1",
    ).unwrap();
}

#[test]
fn same_observed_tail_cannot_be_claimed_twice() {
    let mut store = MemoryStore::new();
    start(&mut store, 43);
    let initial = store.load_durable_tail_position(43).unwrap().unwrap();
    claim_pinned_workflow_recovery(&mut store, 43, initial).unwrap();
    assert!(claim_pinned_workflow_recovery(&mut store, 43, initial).is_err());
    assert_eq!(store.load_durable_tail_position(43).unwrap().unwrap().sequence, 2);
}

#[test]
fn identical_claim_replay_is_never_acknowledged_as_second_owner() {
    let mut store = MemoryStore::new();
    start(&mut store, 44);
    let initial = store.load_durable_tail_position(44).unwrap().unwrap();
    claim_pinned_workflow_recovery(&mut store, 44, initial).unwrap();
    let replay = store.committed_transitions(44).last().unwrap().clone();
    assert_eq!(
        store.commit_transition(replay).unwrap_err().kind(),
        ErrorKind::AlreadyExists,
    );
}

#[test]
fn incomplete_snapshot_cannot_be_claimed_for_recovery() {
    let mut store = MemoryStore::new();
    start(&mut store, 45);
    let observed = store.load_durable_tail_position(45).unwrap().unwrap();
    store.commit_transition(DurableTransition {
        version: DURABLE_TRANSITION_VERSION,
        actor_id: 45,
        activation_epoch: observed.activation_epoch,
        sequence: 2,
        expected_previous_sequence: 1,
        command: None,
        snapshot: None,
        workflow_events: vec![],
        domain_events: vec![],
        durable_effects: vec![],
        outbox: vec![],
    }).unwrap();
    let latest = store.load_durable_tail_position(45).unwrap().unwrap();
    assert_eq!(
        claim_pinned_workflow_recovery(&mut store, 45, latest)
            .unwrap_err().kind(),
        ErrorKind::InvalidData,
    );
}

#[test]
fn bogus_zero_or_maximum_epoch_is_rejected() {
    let mut store = MemoryStore::new();
    start(&mut store, 46);
    assert!(claim_pinned_workflow_recovery(
        &mut store, 46, DurableTailPosition { activation_epoch: 0, sequence: 1 },
    ).is_err());
    assert_eq!(store.load_snapshot(46).unwrap().sequence, 1);
}

#[cfg(feature = "sqlite")]
#[test]
fn competing_libsql_claims_with_one_observed_tail_have_one_winner() {
    use nulang::runtime::LibsqlStore;
    use std::sync::{Arc, Barrier};
    use std::thread;
    use std::time::{SystemTime, UNIX_EPOCH};

    let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let directory = std::env::temp_dir().join(format!(
        "nulang-pinned-claim-race-{}-{nonce}", std::process::id(),
    ));
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("workflow.db");
    let mut first = LibsqlStore::new(&path).unwrap();
    start(&mut first, 47);
    let observed = first.load_durable_tail_position(47).unwrap().unwrap();
    drop(first);

    let stores = [
        LibsqlStore::new(&path).unwrap(),
        LibsqlStore::new(&path).unwrap(),
    ];
    let barrier = Arc::new(Barrier::new(2));
    let handles: Vec<_> = stores.into_iter().map(|mut store| {
        let barrier = Arc::clone(&barrier);
        thread::spawn(move || {
            barrier.wait();
            claim_pinned_workflow_recovery(&mut store, 47, observed)
        })
    }).collect();
    let results: Vec<_> = handles.into_iter().map(|handle| handle.join().unwrap()).collect();
    assert_eq!(
        results.iter().filter(|result| result.is_ok()).count(),
        1,
        "exactly one worker may claim the same committed predecessor: {results:?}",
    );
    let store = LibsqlStore::new(&path).unwrap();
    assert_eq!(store.load_durable_tail_position(47).unwrap().unwrap().sequence, 2);
    std::fs::remove_dir_all(directory).unwrap();
}
