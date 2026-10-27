use nulang::runtime::{JsonFileStore, MemoryStore, PersistenceStore};
use nulang::task_output_commit::{
    CommitDecision, CommitError, DurableTaskOutputStore, TaskOutputStoreError,
};

const SLOT: u64 = 48;
const KEY: &str = "job-48";

#[test]
fn atomic_transitions_keep_staged_output_private_until_commit() {
    let mut store = MemoryStore::new();
    let mut task = DurableTaskOutputStore::new(&mut store, SLOT, KEY).unwrap();
    let owner = task.assign("attempt-1").unwrap();
    assert_eq!(owner.epoch, 1);
    assert_eq!(store.latest_sequence(SLOT), 1);

    let mut task = DurableTaskOutputStore::new(&mut store, SLOT, KEY).unwrap();
    task.stage(&owner, "objects/task-48/attempt-1", [4; 32])
        .unwrap();
    assert!(task.load().unwrap().committed().is_none());
    assert_eq!(store.latest_sequence(SLOT), 2);

    let mut task = DurableTaskOutputStore::new(&mut store, SLOT, KEY).unwrap();
    assert_eq!(task.commit(&owner, [4; 32]).unwrap(), CommitDecision::Won);
    assert_eq!(
        task.load().unwrap().committed().unwrap().artifact_key,
        "objects/task-48/attempt-1"
    );
    assert_eq!(store.latest_sequence(SLOT), 3);
    assert_eq!(
        task.commit(&owner, [4; 32]).unwrap(),
        CommitDecision::AlreadyWon
    );
    assert_eq!(store.latest_sequence(SLOT), 3);
}

#[test]
fn epoch_reassignment_fences_old_worker_without_publishing_its_stage() {
    let mut store = MemoryStore::new();
    let mut task = DurableTaskOutputStore::new(&mut store, SLOT, KEY).unwrap();
    let old = task.assign("attempt-1").unwrap();
    task.stage(&old, "objects/old", [1; 32]).unwrap();
    let new = task.assign("attempt-2").unwrap();

    assert_eq!(new.epoch, old.epoch + 1);
    let sequence = store.latest_sequence(SLOT);
    let mut task = DurableTaskOutputStore::new(&mut store, SLOT, KEY).unwrap();
    assert!(matches!(
        task.commit(&old, [1; 32]),
        Err(TaskOutputStoreError::Semantic(CommitError::Fenced))
    ));
    assert_eq!(store.latest_sequence(SLOT), sequence);
    let mut task = DurableTaskOutputStore::new(&mut store, SLOT, KEY).unwrap();
    task.stage(&new, "objects/new", [2; 32]).unwrap();
    assert_eq!(task.commit(&new, [2; 32]).unwrap(), CommitDecision::Won);
    assert_eq!(task.load().unwrap().committed().unwrap().digest, [2; 32]);
}

#[test]
fn repeated_lease_assignment_and_exact_staging_are_sequence_noops() {
    let mut store = MemoryStore::new();
    let mut task = DurableTaskOutputStore::new(&mut store, SLOT, KEY).unwrap();
    let token = task.assign("attempt-1").unwrap();
    assert_eq!(task.assign("attempt-1").unwrap(), token);
    assert_eq!(store.latest_sequence(SLOT), 1);
    task.stage(&token, "objects/immutable", [9; 32]).unwrap();
    task.stage(&token, "objects/immutable", [9; 32]).unwrap();
    assert_eq!(store.latest_sequence(SLOT), 2);
    assert!(matches!(
        task.stage(&token, "objects/mutated", [9; 32]),
        Err(TaskOutputStoreError::Semantic(CommitError::StageConflict))
    ));
    assert_eq!(store.latest_sequence(SLOT), 2);
}

#[test]
fn task_key_mismatch_is_rejected_before_any_mutation() {
    let mut store = MemoryStore::new();
    let mut first = DurableTaskOutputStore::new(&mut store, SLOT, KEY).unwrap();
    first.assign("owner").unwrap();
    let mismatch = DurableTaskOutputStore::new(&mut store, SLOT, "other-job").unwrap();
    assert!(matches!(
        mismatch.load(),
        Err(TaskOutputStoreError::Corrupt(_))
    ));
    assert_eq!(store.latest_sequence(SLOT), 1);
}

#[test]
fn occupied_actor_slot_and_legacy_non_atomic_store_fail_closed() {
    let mut memory = MemoryStore::new();
    memory
        .append_event(
            SLOT,
            nulang::runtime::EventEntry {
                sequence: 1,
                field_name: "unrelated".into(),
                event_name: "unrelated".into(),
                args: vec![],
                value: nulang::runtime::PersistedValue::Unit,
            },
        )
        .unwrap();
    let mut task = DurableTaskOutputStore::new(&mut memory, SLOT, KEY).unwrap();
    assert!(matches!(task.assign("owner"), Err(TaskOutputStoreError::Corrupt(_))));

    let dir = std::env::temp_dir().join(format!(
        "nulang-task-output-unsupported-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let mut legacy = JsonFileStore::new(&dir).unwrap();
    let task = DurableTaskOutputStore::new(&mut legacy, SLOT, KEY).unwrap();
    assert!(matches!(
        task.load(),
        Err(TaskOutputStoreError::Storage(error))
            if error.kind() == std::io::ErrorKind::Unsupported
    ));
    let _ = std::fs::remove_dir_all(dir);
}

#[cfg(feature = "sqlite")]
#[test]
fn sqlite_reopen_preserves_fenced_attempt_and_single_winner() {
    use nulang::runtime::LibsqlStore;
    use std::time::{SystemTime, UNIX_EPOCH};

    let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "nulang-task-output-reopen-{}-{nonce}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("tasks.db");

    let old = {
        let mut store = LibsqlStore::new(&path).unwrap();
        let mut coordinator = DurableTaskOutputStore::new(&mut store, SLOT, KEY).unwrap();
        let token = coordinator.assign("attempt-old").unwrap();
        coordinator.stage(&token, "objects/old", [1; 32]).unwrap();
        token
    };
    let new = {
        let mut store = LibsqlStore::new(&path).unwrap();
        let mut coordinator = DurableTaskOutputStore::new(&mut store, SLOT, KEY).unwrap();
        let token = coordinator.assign("attempt-new").unwrap();
        assert!(matches!(
            coordinator.commit(&old, [1; 32]),
            Err(TaskOutputStoreError::Semantic(CommitError::Fenced))
        ));
        coordinator.stage(&token, "objects/new", [2; 32]).unwrap();
        assert_eq!(
            coordinator.commit(&token, [2; 32]).unwrap(),
            CommitDecision::Won
        );
        token
    };
    {
        let mut store = LibsqlStore::new(&path).unwrap();
        let mut coordinator = DurableTaskOutputStore::new(&mut store, SLOT, KEY).unwrap();
        assert_eq!(coordinator.load().unwrap().committed().unwrap().digest, [2; 32]);
        assert_eq!(
            coordinator.commit(&new, [2; 32]).unwrap(),
            CommitDecision::AlreadyWon
        );
        assert!(matches!(
            coordinator.assign("attempt-third"),
            Err(TaskOutputStoreError::Semantic(CommitError::AlreadyCommitted))
        ));
    }
    let _ = std::fs::remove_dir_all(dir);
}
