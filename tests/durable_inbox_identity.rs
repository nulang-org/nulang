//! Executable acceptance contract for stable inbound message identity.
//!
//! MemoryStore is the reference implementation. Disk-backed stores must fail
//! closed until inbox receipt and transition are committed transactionally.

use nulang::durable_inbox::DurableInboxCommand;
use nulang::runtime::{
    DurableTransition, JournalEntry, MemoryStore, PersistedValue, PersistenceStore,
    DURABLE_TRANSITION_VERSION,
};

fn transition(actor_id: u64, previous: u64, amount: i64) -> DurableTransition {
    let sequence = previous + 1;
    DurableTransition {
        version: DURABLE_TRANSITION_VERSION,
        actor_id,
        activation_epoch: 1,
        sequence,
        expected_previous_sequence: previous,
        command: Some(JournalEntry {
            sequence,
            behavior_id: 7,
            payload: vec![PersistedValue::Int(amount)],
        }),
        snapshot: None,
        workflow_events: vec![],
        domain_events: vec![],
        durable_effects: vec![],
        outbox: vec![],
    }
}

fn inbox(actor_id: u64, message_key: &str, amount: i64) -> DurableInboxCommand {
    DurableInboxCommand::new(
        "tenant-a",
        "sender-stable-id",
        message_key,
        actor_id,
        7,
        &[PersistedValue::Int(amount)],
    )
    .unwrap()
}

#[test]
fn identity_is_independent_of_receiver_activation_or_commit_sequence() {
    let first = inbox(42, "request-9", 3);
    let retry = inbox(42, "request-9", 3);
    assert_eq!(first.id(), retry.id());
    assert_eq!(first.request_digest(), retry.request_digest());

    // Sequence and receiver activation epoch are deliberately not inputs:
    // the sender must reuse the original logical key after a host failover.
    assert_ne!(first.id(), inbox(42, "request-10", 3).id());
    assert_ne!(first.id(), inbox(43, "request-9", 3).id());
    let other_tenant = DurableInboxCommand::new(
        "tenant-b",
        "sender-stable-id",
        "request-9",
        42,
        7,
        &[PersistedValue::Int(3)],
    )
    .unwrap();
    assert_ne!(first.id(), other_tenant.id());
}

#[test]
fn malformed_sender_identity_and_nonfinite_float_fail_closed() {
    for component in ["", "  "] {
        assert!(DurableInboxCommand::new(
            component,
            "sender",
            "key",
            42,
            7,
            &[PersistedValue::Int(1)],
        )
        .is_err());
        assert!(DurableInboxCommand::new(
            "tenant",
            component,
            "key",
            42,
            7,
            &[PersistedValue::Int(1)],
        )
        .is_err());
        assert!(DurableInboxCommand::new(
            "tenant",
            "sender",
            component,
            42,
            7,
            &[PersistedValue::Int(1)],
        )
        .is_err());
    }
    assert!(DurableInboxCommand::new(
        "tenant",
        "sender",
        "key",
        42,
        7,
        &[PersistedValue::Float(f64::NAN)],
    )
    .is_err());
}

#[test]
fn atomic_memory_admission_persists_receipt_and_journal_together() {
    let mut store = MemoryStore::new();
    let request = inbox(42, "request-9", 3);
    let command = transition(42, 0, 3);

    let first = store
        .commit_inbox_transition(request.clone(), command.clone())
        .expect("commit command and dedup receipt");
    assert_eq!(first.sequence, 1);
    assert_eq!(store.read_journal(42).len(), 1);
    let receipt = store
        .load_durable_inbox_receipt(request.id())
        .expect("reference backend supports receipt lookup")
        .expect("receipt persisted");
    assert_eq!(receipt.commit, first);
    assert_eq!(receipt.request_digest, request.request_digest());

    let retry = store
        .commit_inbox_transition(request, command)
        .expect("lost acknowledgment retry");
    assert_eq!(retry, first);
    assert_eq!(store.read_journal(42).len(), 1);
    assert_eq!(store.committed_transitions(42).len(), 1);
}

#[test]
fn retrying_old_message_after_newer_commits_returns_original_receipt() {
    let mut store = MemoryStore::new();
    let first_msg = inbox(42, "request-9", 3);
    let first_transition = transition(42, 0, 3);
    let first = store
        .commit_inbox_transition(first_msg.clone(), first_transition.clone())
        .unwrap();
    let second = store
        .commit_inbox_transition(inbox(42, "request-10", 5), transition(42, 1, 5))
        .unwrap();
    assert_eq!(second.sequence, 2);

    let retried = store
        .commit_inbox_transition(first_msg, first_transition)
        .unwrap();
    assert_eq!(retried, first);
    assert_eq!(store.durable_tail(42).unwrap().sequence, 2);
    assert_eq!(store.read_journal(42).len(), 2);
}

#[test]
fn reused_logical_identity_with_changed_payload_is_rejected() {
    let mut store = MemoryStore::new();
    store
        .commit_inbox_transition(inbox(42, "request-9", 3), transition(42, 0, 3))
        .unwrap();
    let conflict = store
        .commit_inbox_transition(inbox(42, "request-9", 99), transition(42, 1, 99))
        .expect_err("same logical ID must never mean different input");
    assert_eq!(conflict.kind(), std::io::ErrorKind::AlreadyExists);
    assert_eq!(store.read_journal(42).len(), 1);
}

#[test]
fn untrusted_metadata_mismatch_never_mutates_persistence() {
    let mut store = MemoryStore::new();
    let request = inbox(42, "request-9", 3);
    let err = store
        .commit_inbox_transition(request.clone(), transition(42, 0, 4))
        .unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    let err = store
        .commit_inbox_transition(request.clone(), transition(43, 0, 3))
        .unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    let mut wrong_behavior = transition(42, 0, 3);
    wrong_behavior.command.as_mut().unwrap().behavior_id = 8;
    assert_eq!(
        store
            .commit_inbox_transition(request.clone(), wrong_behavior)
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::InvalidInput
    );
    assert!(store.read_journal(42).is_empty());
    assert!(store
        .load_durable_inbox_receipt(request.id())
        .unwrap()
        .is_none());
}

#[test]
fn ordinary_atomic_commit_cannot_be_retroactively_claimed_as_inbox_commit() {
    let mut store = MemoryStore::new();
    let already_committed = transition(42, 0, 3);
    store.commit_transition(already_committed.clone()).unwrap();

    let err = store
        .commit_inbox_transition(inbox(42, "request-9", 3), already_committed)
        .expect_err("inbox receipt must commit atomically with the command");
    assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
    assert_eq!(store.read_journal(42).len(), 1);
}

#[cfg(feature = "sqlite")]
#[test]
fn libsql_must_report_unsupported_until_transactional_inbox_is_implemented() {
    use nulang::runtime::LibsqlStore;
    let mut store = LibsqlStore::in_memory().unwrap();
    let request = inbox(42, "request-9", 3);
    assert_eq!(
        store
            .commit_inbox_transition(request.clone(), transition(42, 0, 3))
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::Unsupported
    );
    assert_eq!(
        store
            .load_durable_inbox_receipt(request.id())
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::Unsupported
    );
    assert!(store.read_journal(42).is_empty());
}

#[test]
fn distinct_message_keys_do_not_conflate_identical_business_commands() {
    let mut store = MemoryStore::new();
    let first = inbox(42, "order-100", 3);
    let second = inbox(42, "order-101", 3);
    assert_ne!(first.id(), second.id());
    assert_eq!(first.request_digest(), second.request_digest());

    store
        .commit_inbox_transition(first, transition(42, 0, 3))
        .unwrap();
    store
        .commit_inbox_transition(second, transition(42, 1, 3))
        .unwrap();
    assert_eq!(store.read_journal(42).len(), 2);
}

#[test]
fn tagged_request_digest_distinguishes_values_and_float_bits() {
    let key = |value: PersistedValue| {
        DurableInboxCommand::new("tenant", "sender", "same", 42, 7, &[value])
            .unwrap()
            .request_digest()
    };
    assert_ne!(key(PersistedValue::Int(1)), key(PersistedValue::Bool(true)));
    assert_ne!(
        key(PersistedValue::Float(0.0)),
        key(PersistedValue::Float(-0.0))
    );
    assert_ne!(
        key(PersistedValue::String("1".to_string())),
        key(PersistedValue::Int(1))
    );
}
