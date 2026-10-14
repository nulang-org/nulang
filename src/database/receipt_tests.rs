//! Contract tests for single-tablet durable idempotency receipts.
//!
//! These exercise real WAL and checkpoint files, including an ambiguous append
//! where the caller sees an error but the durable record survives reopening.
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use super::interruption::{with_interruption, StorageInterruptionPoint};
use super::store::{WalBackedError, WalBackedTablet};
use super::tablet::{
    KeyRange, TabletDescriptor, TabletId, TabletMutation, TransactionId,
};

static NEXT_PATH: AtomicU64 = AtomicU64::new(1);

fn wal_path() -> PathBuf {
    std::env::temp_dir().join(format!(
        "nulang_nudb_receipts_{}_{}.wal",
        std::process::id(),
        NEXT_PATH.fetch_add(1, Ordering::Relaxed)
    ))
}

fn descriptor() -> TabletDescriptor {
    TabletDescriptor::new(
        TabletId::new(901).unwrap(),
        KeyRange::new(b"a".to_vec(), Some(b"z".to_vec())).unwrap(),
        4,
    )
    .unwrap()
}

fn put(key: &[u8], value: &[u8]) -> Vec<TabletMutation> {
    vec![TabletMutation::Put {
        key: key.to_vec(),
        value: value.to_vec(),
    }]
}

fn cleanup(path: &std::path::Path) {
    let _ = fs::remove_file(path);
    let _ = fs::remove_file(path.with_extension("checkpoint"));
}

#[test]
fn repeated_transaction_returns_original_receipt_after_restart() {
    let path = wal_path();
    cleanup(&path);
    let id = TransactionId::new([1; 16]);
    let mutation = put(b"k", b"first");
    let mut tablet = WalBackedTablet::open(descriptor(), &path).unwrap();

    let receipt = tablet.commit_idempotent(id, mutation.clone()).unwrap();
    assert_eq!(receipt.id(), id);
    assert_eq!(receipt.sequence(), 1);

    // A retry must not append another version even on the same live handle.
    assert_eq!(tablet.commit_idempotent(id, mutation.clone()).unwrap(), receipt);
    assert_eq!(tablet.current_sequence(), 1);
    drop(tablet);

    let mut tablet = WalBackedTablet::open(descriptor(), &path).unwrap();
    assert_eq!(tablet.commit_idempotent(id, mutation).unwrap(), receipt);
    assert_eq!(tablet.current_sequence(), 1);
    assert_eq!(tablet.read_latest(b"k"), Some(&b"first"[..]));
    drop(tablet);
    cleanup(&path);
}

#[test]
fn one_transaction_id_cannot_be_reused_with_different_mutations() {
    let path = wal_path();
    cleanup(&path);
    let id = TransactionId::new([2; 16]);
    let mut tablet = WalBackedTablet::open(descriptor(), &path).unwrap();

    tablet.commit_idempotent(id, put(b"k", b"first")).unwrap();
    assert!(matches!(
        tablet.commit_idempotent(id, put(b"k", b"changed")),
        Err(WalBackedError::IdempotencyConflict(conflicting)) if conflicting == id
    ));
    assert_eq!(tablet.current_sequence(), 1);
    assert_eq!(tablet.read_latest(b"k"), Some(&b"first"[..]));
    drop(tablet);

    let mut tablet = WalBackedTablet::open(descriptor(), &path).unwrap();
    assert!(matches!(
        tablet.commit_idempotent(id, put(b"j", b"other-key")),
        Err(WalBackedError::IdempotencyConflict(conflicting)) if conflicting == id
    ));
    assert_eq!(tablet.current_sequence(), 1);
    drop(tablet);
    cleanup(&path);
}

#[test]
fn checkpoint_reclamation_preserves_all_commit_receipts() {
    let path = wal_path();
    cleanup(&path);
    let first = TransactionId::new([3; 16]);
    let second = TransactionId::new([4; 16]);
    let mut tablet = WalBackedTablet::open(descriptor(), &path).unwrap();

    let one = tablet.commit_idempotent(first, put(b"f", b"one")).unwrap();
    let two = tablet.commit_idempotent(second, put(b"g", b"two")).unwrap();
    tablet.checkpoint().unwrap();
    drop(tablet);

    let mut tablet = WalBackedTablet::open(descriptor(), &path).unwrap();
    assert_eq!(tablet.commit_idempotent(first, put(b"f", b"one")).unwrap(), one);
    assert_eq!(tablet.commit_idempotent(second, put(b"g", b"two")).unwrap(), two);
    assert_eq!(tablet.current_sequence(), 2);

    let three = tablet
        .commit_idempotent(TransactionId::new([5; 16]), put(b"h", b"three"))
        .unwrap();
    assert_eq!(three.sequence(), 3);
    drop(tablet);

    let mut tablet = WalBackedTablet::open(descriptor(), &path).unwrap();
    assert_eq!(tablet.commit_idempotent(first, put(b"f", b"one")).unwrap(), one);
    assert_eq!(tablet.commit_idempotent(second, put(b"g", b"two")).unwrap(), two);
    assert_eq!(tablet.current_sequence(), 3);
    drop(tablet);
    cleanup(&path);
}

#[test]
fn ambiguous_error_after_wal_sync_can_be_reconciled_by_transaction_id() {
    let path = wal_path();
    cleanup(&path);
    let id = TransactionId::new([6; 16]);
    let mutation = put(b"k", b"durable");
    let mut tablet = WalBackedTablet::open(descriptor(), &path).unwrap();

    let error = with_interruption(StorageInterruptionPoint::WalAfterSync, || {
        tablet.commit_idempotent(id, mutation.clone())
    });
    assert!(error.is_err());
    drop(tablet);

    // A durable WAL frame exists even though commit_idempotent returned Err.
    // The transaction identity must be replayed together with its mutations.
    let mut tablet = WalBackedTablet::open(descriptor(), &path).unwrap();
    let receipt = tablet.commit_idempotent(id, mutation).unwrap();
    assert_eq!(receipt.sequence(), 1);
    assert_eq!(tablet.current_sequence(), 1);
    assert_eq!(tablet.read_latest(b"k"), Some(&b"durable"[..]));
    drop(tablet);
    cleanup(&path);
}

#[test]
fn legacy_unidentified_writes_still_recover_alongside_idempotent_commits() {
    let path = wal_path();
    cleanup(&path);
    let mut tablet = WalBackedTablet::open(descriptor(), &path).unwrap();
    let write = tablet.prepare_write(4, 0, put(b"b", b"legacy")).unwrap();
    assert_eq!(tablet.commit(write).unwrap(), 1);
    let id = TransactionId::new([7; 16]);
    let receipt = tablet.commit_idempotent(id, put(b"c", b"new")).unwrap();
    assert_eq!(receipt.sequence(), 2);
    tablet.checkpoint().unwrap();
    drop(tablet);

    let mut tablet = WalBackedTablet::open(descriptor(), &path).unwrap();
    assert_eq!(tablet.read_latest(b"b"), Some(&b"legacy"[..]));
    assert_eq!(tablet.commit_idempotent(id, put(b"c", b"new")).unwrap(), receipt);
    assert_eq!(tablet.current_sequence(), 2);
    drop(tablet);
    cleanup(&path);
}

#[test]
fn torn_wal_frame_does_not_claim_transaction_id() {
    let path = wal_path();
    cleanup(&path);
    let id = TransactionId::new([8; 16]);
    let mutation = put(b"k", b"retry-after-torn-frame");
    let mut tablet = WalBackedTablet::open(descriptor(), &path).unwrap();

    assert!(with_interruption(StorageInterruptionPoint::WalAfterPayload, || {
        tablet.commit_idempotent(id, mutation.clone())
    })
    .is_err());
    drop(tablet);

    // Reopen discards the incomplete frame; the same logical transaction can
    // now commit exactly once, with its original ID and payload.
    let mut tablet = WalBackedTablet::open(descriptor(), &path).unwrap();
    assert_eq!(tablet.current_sequence(), 0);
    let receipt = tablet.commit_idempotent(id, mutation.clone()).unwrap();
    assert_eq!(receipt.sequence(), 1);
    assert_eq!(tablet.commit_idempotent(id, mutation).unwrap(), receipt);
    assert_eq!(tablet.current_sequence(), 1);
    drop(tablet);
    cleanup(&path);
}

#[test]
fn published_checkpoint_preserves_receipt_after_interrupted_publication() {
    let path = wal_path();
    cleanup(&path);
    let id = TransactionId::new([9; 16]);
    let mutation = put(b"k", b"checkpoint");
    let mut tablet = WalBackedTablet::open(descriptor(), &path).unwrap();
    let receipt = tablet.commit_idempotent(id, mutation.clone()).unwrap();

    // The checkpoint rename completed before the injected error. Both the
    // checkpoint and original WAL must remain individually safe to reopen.
    assert!(with_interruption(StorageInterruptionPoint::CheckpointAfterRename, || {
        tablet.publish_checkpoint()
    })
    .is_err());
    drop(tablet);

    let mut tablet = WalBackedTablet::open(descriptor(), &path).unwrap();
    assert_eq!(tablet.commit_idempotent(id, mutation).unwrap(), receipt);
    assert_eq!(tablet.current_sequence(), 1);
    drop(tablet);
    cleanup(&path);
}
