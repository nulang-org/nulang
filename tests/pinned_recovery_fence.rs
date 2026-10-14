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
fn former_owner_cannot_commit_durable_work_after_a_new_epoch_is_claimed() {
    let mut store = MemoryStore::new();
    start(&mut store, 51);
    let observed = store.load_durable_tail_position(51).unwrap().unwrap();
    let claim = claim_pinned_workflow_recovery(&mut store, 51, observed).unwrap();

    let stale = store.commit_transition(DurableTransition {
        version: DURABLE_TRANSITION_VERSION,
        actor_id: 51,
        activation_epoch: observed.activation_epoch,
        sequence: claim.sequence + 1,
        expected_previous_sequence: claim.sequence,
        command: None,
        snapshot: None,
        workflow_events: vec![],
        domain_events: vec![],
        durable_effects: vec![],
        outbox: vec![],
    }).unwrap_err();
    assert_eq!(stale.kind(), ErrorKind::PermissionDenied);
    assert_eq!(
        store.load_durable_tail_position(51).unwrap().unwrap().activation_epoch,
        claim.activation_epoch,
    );
}

#[test]
fn later_claim_replaces_previous_epoch_and_fences_the_previous_claimant() {
    let mut store = MemoryStore::new();
    start(&mut store, 52);
    let original = store.load_durable_tail_position(52).unwrap().unwrap();
    let first = claim_pinned_workflow_recovery(&mut store, 52, original).unwrap();
    let new_tail = store.load_durable_tail_position(52).unwrap().unwrap();
    let second = claim_pinned_workflow_recovery(&mut store, 52, new_tail).unwrap();
    assert_eq!(second.activation_epoch, first.activation_epoch + 1);
    assert_eq!(second.sequence, first.sequence + 1);
    assert!(claim_pinned_workflow_recovery(&mut store, 52, new_tail).is_err());
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

#[cfg(feature = "sqlite")]
#[test]
fn runtime_refuses_claim_if_another_worker_wins_during_artifact_resolution() {
    use nulang::runtime::{
        ImmutablePinnedArtifactResolver, LibsqlStore, PinnedArtifactBundle, Runtime,
    };
    use std::io;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct RacingResolver {
        path: PathBuf,
        bundle: PinnedArtifactBundle,
    }

    impl ImmutablePinnedArtifactResolver for RacingResolver {
        fn resolve(&self, digest: &str) -> io::Result<Option<PinnedArtifactBundle>> {
            if digest != self.bundle.manifest.artifact.digest {
                return Ok(None);
            }
            // Simulate another worker claiming the exact old committed tail
            // while this worker is resolving the pinned immutable artifact.
            let mut contender = LibsqlStore::new(&self.path)?;
            let observed = contender.load_durable_tail_position(48)?
                .expect("committed pinned workflow");
            claim_pinned_workflow_recovery(&mut contender, 48, observed)?;
            Ok(Some(self.bundle.clone()))
        }
    }

    let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let directory = std::env::temp_dir().join(format!(
        "nulang-fenced-resolver-race-{}-{nonce}", std::process::id(),
    ));
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("workflow.db");
    {
        let mut store = LibsqlStore::new(&path).unwrap();
        start(&mut store, 48);
    }

    let bytes = b"fenced-workflow-v1";
    let resolver = RacingResolver {
        path: path.clone(),
        bundle: PinnedArtifactBundle {
            manifest: behavior(bytes),
            executable: bytes.to_vec(),
        },
    };
    let mut runtime = Runtime::new();
    runtime.persistence = Box::new(LibsqlStore::new(&path).unwrap());
    let error = runtime.recover_pinned_workflow_actor(
        48, "payments/one", &resolver,
    ).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::PermissionDenied);
    assert!(!runtime.actors.contains_key(&48));
    assert_eq!(
        runtime.persistence.load_durable_tail_position(48)
            .unwrap().unwrap().sequence,
        2,
    );
    drop(runtime);
    std::fs::remove_dir_all(directory).unwrap();
}

#[cfg(feature = "sqlite")]
#[test]
fn runtime_recovers_with_claimed_epoch_from_original_pinned_artifact() {
    use nulang::runtime::{
        ImmutablePinnedArtifactResolver, LibsqlStore, PinnedArtifactBundle, Runtime,
    };
    use std::io;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct Resolver(PinnedArtifactBundle);
    impl ImmutablePinnedArtifactResolver for Resolver {
        fn resolve(&self, digest: &str) -> io::Result<Option<PinnedArtifactBundle>> {
            Ok((digest == self.0.manifest.artifact.digest).then(|| self.0.clone()))
        }
    }

    let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let directory = std::env::temp_dir().join(format!(
        "nulang-epoch-restart-{}-{nonce}", std::process::id(),
    ));
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("workflow.db");
    {
        let mut store = LibsqlStore::new(&path).unwrap();
        start(&mut store, 49);
    }

    let bytes = b"fenced-workflow-v1";
    let mut runtime = Runtime::new();
    runtime.persistence = Box::new(LibsqlStore::new(&path).unwrap());
    assert_eq!(
        runtime.recover_pinned_workflow_actor(
            49, "payments/one",
            &Resolver(PinnedArtifactBundle {
                manifest: behavior(bytes),
                executable: bytes.to_vec(),
            }),
        ).unwrap(),
        49,
    );
    let actor = &runtime.actors[&49];
    assert_eq!(actor.activation_epoch, 2);
    assert_eq!(actor.sequence, 2);
    drop(runtime);
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn memory_checked_snapshot_and_journal_reads_are_lossless() {
    use nulang::runtime::JournalEntry;

    let mut store = MemoryStore::new();
    start(&mut store, 61);
    store.append_journal(
        61,
        JournalEntry {
            sequence: 1,
            behavior_id: 0,
            payload: vec![],
        },
    ).unwrap();

    let snapshot = store.load_snapshot_checked(61).unwrap().unwrap();
    assert_eq!(snapshot.actor_id, 61);
    assert_eq!(snapshot.sequence, 1);
    assert_eq!(store.read_journal_checked(61).unwrap().len(), 1);
}

#[cfg(feature = "sqlite")]
#[test]
fn corrupt_libsql_snapshot_is_not_interpreted_as_missing_durable_state() {
    use nulang::runtime::LibsqlStore;

    let mut store = LibsqlStore::in_memory().unwrap();
    start(&mut store, 62);
    store.query(
        "UPDATE snapshots SET state = 'broken-json' WHERE actor_id = 62",
        &[],
    ).unwrap();

    assert_eq!(
        store.load_snapshot_checked(62).unwrap_err().kind(),
        ErrorKind::InvalidData,
    );
    let tail = store.load_durable_tail_position(62).unwrap().unwrap();
    assert_eq!(
        claim_pinned_workflow_recovery(&mut store, 62, tail)
            .unwrap_err().kind(),
        ErrorKind::InvalidData,
    );
    assert_eq!(store.load_durable_tail_position(62).unwrap().unwrap(), tail);
}

#[cfg(feature = "sqlite")]
#[test]
fn corrupt_libsql_command_journal_prevents_fenced_recovery() {
    use nulang::runtime::{JournalEntry, LibsqlStore};

    let mut store = LibsqlStore::in_memory().unwrap();
    start(&mut store, 63);
    store.append_journal(63, JournalEntry {
        sequence: 1,
        behavior_id: 0,
        payload: vec![],
    }).unwrap();
    store.query(
        "UPDATE journal SET payload = 'broken-json' WHERE actor_id = 63",
        &[],
    ).unwrap();

    assert_eq!(
        store.read_journal_checked(63).unwrap_err().kind(),
        ErrorKind::InvalidData,
    );
    let tail = store.load_durable_tail_position(63).unwrap().unwrap();
    assert_eq!(
        claim_pinned_workflow_recovery(&mut store, 63, tail)
            .unwrap_err().kind(),
        ErrorKind::InvalidData,
    );
    assert_eq!(store.load_durable_tail_position(63).unwrap().unwrap(), tail);
}

#[cfg(feature = "sqlite")]
#[test]
fn malformed_libsql_command_sequence_or_behavior_id_is_rejected() {
    use nulang::runtime::{JournalEntry, LibsqlStore};

    let mut store = LibsqlStore::in_memory().unwrap();
    start(&mut store, 64);
    store.append_journal(64, JournalEntry {
        sequence: 1,
        behavior_id: 0,
        payload: vec![],
    }).unwrap();
    store.query(
        "UPDATE journal SET behavior_id = 65536 WHERE actor_id = 64",
        &[],
    ).unwrap();

    assert_eq!(
        store.read_journal_checked(64).unwrap_err().kind(),
        ErrorKind::InvalidData,
    );
}

#[cfg(feature = "sqlite")]
#[test]
fn corrupt_command_journal_blocks_runtime_pinned_recovery_before_epoch_claim() {
    use nulang::runtime::{
        ImmutablePinnedArtifactResolver, JournalEntry, LibsqlStore,
        PinnedArtifactBundle, Runtime,
    };
    use std::io;

    struct Resolver(PinnedArtifactBundle);
    impl ImmutablePinnedArtifactResolver for Resolver {
        fn resolve(&self, digest: &str) -> io::Result<Option<PinnedArtifactBundle>> {
            Ok((digest == self.0.manifest.artifact.digest).then(|| self.0.clone()))
        }
    }

    let mut store = LibsqlStore::in_memory().unwrap();
    start(&mut store, 65);
    store.append_journal(65, JournalEntry {
        sequence: 1,
        behavior_id: 0,
        payload: vec![],
    }).unwrap();
    store.query(
        "UPDATE journal SET payload = 'broken-json' WHERE actor_id = 65",
        &[],
    ).unwrap();

    let initial = store.load_durable_tail_position(65).unwrap().unwrap();
    let mut runtime = Runtime::new();
    runtime.persistence = Box::new(store);
    let bytes = b"fenced-workflow-v1";
    assert_eq!(
        runtime.recover_pinned_workflow_actor(
            65,
            "payments/one",
            &Resolver(PinnedArtifactBundle {
                manifest: behavior(bytes),
                executable: bytes.to_vec(),
            }),
        ).unwrap_err().kind(),
        ErrorKind::InvalidData,
    );
    assert!(!runtime.actors.contains_key(&65));
    assert_eq!(
        runtime.persistence.load_durable_tail_position(65).unwrap().unwrap(),
        initial,
    );
}
