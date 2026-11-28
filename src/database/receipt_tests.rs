//! Contract tests for single-tablet durable idempotency receipts.
//!
//! These exercise real WAL and checkpoint files, including an ambiguous append
//! where the caller sees an error but the durable record survives reopening.
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use super::interruption::{with_interruption, StorageInterruptionPoint};
use super::store::{WalBackedError, WalBackedTablet};
use super::tablet::{KeyRange, TabletDescriptor, TabletId, TabletMutation, TransactionId};

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
    assert_eq!(
        tablet.commit_idempotent(id, mutation.clone()).unwrap(),
        receipt
    );
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
    assert_eq!(
        tablet.commit_idempotent(first, put(b"f", b"one")).unwrap(),
        one
    );
    assert_eq!(
        tablet.commit_idempotent(second, put(b"g", b"two")).unwrap(),
        two
    );
    assert_eq!(tablet.current_sequence(), 2);

    let three = tablet
        .commit_idempotent(TransactionId::new([5; 16]), put(b"h", b"three"))
        .unwrap();
    assert_eq!(three.sequence(), 3);
    drop(tablet);

    let mut tablet = WalBackedTablet::open(descriptor(), &path).unwrap();
    assert_eq!(
        tablet.commit_idempotent(first, put(b"f", b"one")).unwrap(),
        one
    );
    assert_eq!(
        tablet.commit_idempotent(second, put(b"g", b"two")).unwrap(),
        two
    );
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
    assert_eq!(
        tablet.commit_idempotent(id, put(b"c", b"new")).unwrap(),
        receipt
    );
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

    assert!(
        with_interruption(StorageInterruptionPoint::WalAfterPayload, || {
            tablet.commit_idempotent(id, mutation.clone())
        })
        .is_err()
    );
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
    assert!(
        with_interruption(StorageInterruptionPoint::CheckpointAfterRename, || {
            tablet.publish_checkpoint()
        })
        .is_err()
    );
    drop(tablet);

    let mut tablet = WalBackedTablet::open(descriptor(), &path).unwrap();
    assert_eq!(tablet.commit_idempotent(id, mutation).unwrap(), receipt);
    assert_eq!(tablet.current_sequence(), 1);
    drop(tablet);
    cleanup(&path);
}

#[test]
fn receipts_survive_snapshot_pruning_and_reject_conflicting_retry() {
    let path = wal_path();
    let id = TransactionId::new([10; 16]);
    let mut tablet = WalBackedTablet::open(descriptor(), &path).unwrap();
    let receipt = tablet
        .commit_idempotent(id, put(b"k", b"original"))
        .unwrap();
    tablet
        .commit_idempotent(TransactionId::new([11; 16]), put(b"k", b"new"))
        .unwrap();
    tablet.reclaim_history_before(2).unwrap();
    drop(tablet);
    let mut tablet = WalBackedTablet::open(descriptor(), &path).unwrap();
    assert_eq!(
        tablet
            .commit_idempotent(id, put(b"k", b"original"))
            .unwrap(),
        receipt
    );
    assert!(matches!(
        tablet.commit_idempotent(id, put(b"k", b"changed")),
        Err(WalBackedError::IdempotencyConflict(_))
    ));
    assert_eq!(tablet.current_sequence(), 2);
    assert_eq!(tablet.read_latest(b"k"), Some(&b"new"[..]));
    drop(tablet);
    cleanup(&path);
}

#[test]
fn ambiguous_append_blocks_receipt_retries_until_reopen() {
    let path = wal_path();
    let mut tablet = WalBackedTablet::open(descriptor(), &path).unwrap();
    let existing = TransactionId::new([12; 16]);
    let uncertain = TransactionId::new([13; 16]);
    tablet
        .commit_idempotent(existing, put(b"k", b"first"))
        .unwrap();
    assert!(
        with_interruption(StorageInterruptionPoint::WalAfterSync, || {
            tablet.commit_idempotent(uncertain, put(b"k", b"second"))
        })
        .is_err()
    );
    assert!(matches!(
        tablet.commit_idempotent(existing, put(b"k", b"first")),
        Err(WalBackedError::Wal(super::wal::WalError::Poisoned))
    ));
    assert!(matches!(
        tablet.commit_idempotent(uncertain, put(b"k", b"second")),
        Err(WalBackedError::Wal(super::wal::WalError::Poisoned))
    ));
    drop(tablet);
    let mut tablet = WalBackedTablet::open(descriptor(), &path).unwrap();
    assert_eq!(
        tablet
            .commit_idempotent(uncertain, put(b"k", b"second"))
            .unwrap()
            .sequence(),
        2
    );
    assert_eq!(tablet.current_sequence(), 2);
    drop(tablet);
    cleanup(&path);
}

fn edit_checkpoint(path: &std::path::Path, change: impl FnOnce(&mut serde_json::Value)) -> Vec<u8> {
    let file = path.with_extension("checkpoint");
    let bytes = fs::read(&file).unwrap();
    let size = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
    let mut disk: serde_json::Value = serde_json::from_slice(&bytes[12..12 + size]).unwrap();
    change(&mut disk);
    let payload = serde_json::to_vec(&disk).unwrap();
    let mut encoded = bytes[..8].to_vec();
    encoded.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    encoded.extend_from_slice(&payload);
    encoded.extend_from_slice(blake3::hash(&payload).as_bytes());
    fs::write(file, &encoded).unwrap();
    encoded
}

#[test]
fn checkpoint_receipt_disagreement_with_retained_wal_fails_closed() {
    let path = wal_path();
    let mut tablet = WalBackedTablet::open(descriptor(), &path).unwrap();
    let first = TransactionId::new([14; 16]);
    let second = TransactionId::new([15; 16]);
    tablet
        .commit_idempotent(first, put(b"k", b"first"))
        .unwrap();
    tablet
        .commit_idempotent(second, put(b"k", b"second"))
        .unwrap();
    tablet.publish_checkpoint().unwrap();
    drop(tablet);
    let file = path.with_extension("checkpoint");
    let original = fs::read(&file).unwrap();
    for variant in 0..5 {
        fs::write(&file, &original).unwrap();
        let encoded = edit_checkpoint(&path, |disk| {
            assert_eq!(disk["version"], 3);
            let receipts = disk["state"]["receipts"].as_array_mut().unwrap();
            match variant {
                0 => {
                    receipts.remove(0);
                }
                1 => {
                    receipts[0]["digest"][0] =
                        serde_json::json!(255 - receipts[0]["digest"][0].as_u64().unwrap());
                }
                2 => {
                    receipts[1]["sequence"] = receipts[0]["sequence"].clone();
                }
                3 => {
                    receipts[1]["id"] = receipts[0]["id"].clone();
                }
                _ => {
                    disk["version"] = 1.into();
                }
            }
        });
        assert!(
            WalBackedTablet::open(descriptor(), &path).is_err(),
            "variant={variant}"
        );
        assert_eq!(fs::read(&file).unwrap(), encoded);
    }
    fs::write(&file, original).unwrap();
    let mut tablet = WalBackedTablet::open(descriptor(), &path).unwrap();
    assert_eq!(
        tablet
            .commit_idempotent(first, put(b"k", b"first"))
            .unwrap()
            .sequence(),
        1
    );
    drop(tablet);
    cleanup(&path);
}

#[test]
fn pruned_receipt_checkpoint_cannot_omit_floor() {
    let path = wal_path();
    let mut tablet = WalBackedTablet::open(descriptor(), &path).unwrap();
    tablet
        .commit_idempotent(TransactionId::new([16; 16]), put(b"k", b"first"))
        .unwrap();
    tablet.reclaim_history_before(1).unwrap();
    drop(tablet);
    let file = path.with_extension("checkpoint");
    let original = fs::read(&file).unwrap();
    let encoded = edit_checkpoint(&path, |disk| {
        disk["state"]
            .as_object_mut()
            .unwrap()
            .remove("oldest_retained_snapshot");
    });
    assert!(WalBackedTablet::open(descriptor(), &path).is_err());
    assert_eq!(fs::read(&file).unwrap(), encoded);
    fs::write(file, original).unwrap();
    let tablet = WalBackedTablet::open(descriptor(), &path).unwrap();
    assert_eq!(tablet.oldest_retained_snapshot(), 1);
    drop(tablet);
    cleanup(&path);
}
