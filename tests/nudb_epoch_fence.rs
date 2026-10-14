//! Durable single-tablet ownership epoch barriers.
//!
//! Tests prove restart-safe fencing, not authority distribution or consensus.
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use nulang::database::store::{WalBackedError, WalBackedTablet};
use nulang::database::tablet::{
    KeyRange, TabletDescriptor, TabletError, TabletId, TabletMutation, TransactionId, TabletWrite,
};
use nulang::database::wal::{FileWal, WalError};

static NEXT_TEST: AtomicU64 = AtomicU64::new(1);

fn path() -> PathBuf {
    std::env::temp_dir().join(format!(
        "nudb_epoch_fence_{}_{}.wal",
        std::process::id(),
        NEXT_TEST.fetch_add(1, Ordering::Relaxed)
    ))
}

fn descriptor(epoch: u64) -> TabletDescriptor {
    TabletDescriptor::new(
        TabletId::new(600).unwrap(),
        KeyRange::new(b"a".to_vec(), Some(b"z".to_vec())).unwrap(),
        epoch,
    )
    .unwrap()
}

fn cleanup(path: &std::path::Path) {
    let _ = fs::remove_file(path);
    let _ = fs::remove_file(path.with_extension("checkpoint"));
    let mut sidecar = path.as_os_str().to_os_string();
    sidecar.push(".lock");
    let _ = fs::remove_file(PathBuf::from(sidecar));
}

fn put(key: &[u8], value: &[u8]) -> Vec<TabletMutation> {
    vec![TabletMutation::Put {
        key: key.to_vec(),
        value: value.to_vec(),
    }]
}

#[test]
fn epoch_handoff_rejects_prepared_writes_and_survives_checkpoint() {
    let path = path();
    cleanup(&path);
    let mut tablet = WalBackedTablet::open(descriptor(7), &path).unwrap();
    let id = TransactionId::new([51; 16]);
    let receipt = tablet.commit_idempotent(id, put(b"k", b"v1")).unwrap();

    let stale_write = tablet.prepare_write(7, 1, put(b"s", b"stale")).unwrap();
    let fence_sequence = tablet.advance_ownership_epoch(7, 9).unwrap();
    assert_eq!(fence_sequence, 2);
    assert_eq!(tablet.descriptor().ownership_epoch(), 9);
    assert_eq!(
        tablet.commit(stale_write).unwrap_err(),
        WalBackedError::Tablet(TabletError::StaleEpoch {
            current: 9,
            presented: 7,
        })
    );
    assert_eq!(tablet.current_sequence(), 2);
    assert_eq!(tablet.commit_idempotent(id, put(b"k", b"v1")).unwrap(), receipt);

    let new_receipt = tablet
        .commit_idempotent(TransactionId::new([52; 16]), put(b"n", b"new"))
        .unwrap();
    assert_eq!(new_receipt.sequence(), 3);
    tablet.checkpoint().unwrap();
    drop(tablet);

    assert!(WalBackedTablet::open(descriptor(7), &path).is_err());
    assert!(matches!(
        WalBackedTablet::open(descriptor(10), &path),
        Err(WalBackedError::Wal(WalError::UnknownOwnershipEpoch {
            durable: 9,
            presented: 10,
        }))
    ));

    let mut reopened = WalBackedTablet::open(descriptor(9), &path).unwrap();
    assert_eq!(reopened.current_sequence(), 3);
    assert_eq!(reopened.read_latest(b"k"), Some(&b"v1"[..]));
    assert_eq!(reopened.commit_idempotent(id, put(b"k", b"v1")).unwrap(), receipt);
    assert_eq!(reopened.current_sequence(), 3);
    assert_eq!(
        reopened.advance_ownership_epoch(7, 10).unwrap_err(),
        WalBackedError::Tablet(TabletError::StaleEpoch {
            current: 9,
            presented: 7,
        })
    );
    drop(reopened);
    cleanup(&path);
}

#[test]
fn future_descriptor_cannot_implicitly_bump_durable_epoch() {
    let path = path();
    cleanup(&path);
    let mut tablet = WalBackedTablet::open(descriptor(3), &path).unwrap();
    let write = tablet.prepare_write(3, 0, put(b"k", b"value")).unwrap();
    tablet.commit(write).unwrap();
    drop(tablet);

    assert!(matches!(
        WalBackedTablet::open(descriptor(4), &path),
        Err(WalBackedError::Wal(WalError::UnknownOwnershipEpoch {
            durable: 3,
            presented: 4,
        }))
    ));
    let tablet = WalBackedTablet::open(descriptor(3), &path).unwrap();
    assert_eq!(tablet.current_sequence(), 1);
    drop(tablet);
    cleanup(&path);
}

#[test]
fn direct_wal_append_cannot_implicitly_advance_epoch() {
    let path = path();
    cleanup(&path);
    let mut wal = FileWal::open(&path).unwrap();
    let original = TabletWrite::prepare(&descriptor(3), 3, 0, 0, put(b"k", b"value")).unwrap();
    wal.append_write(&original).unwrap();
    let forged = TabletWrite::prepare(&descriptor(4), 4, 1, 1, put(b"y", b"future")).unwrap();
    assert_eq!(
        wal.append_write(&forged).unwrap_err(),
        WalError::UnfencedEpochAdvance {
            durable: 3,
            presented: 4,
        }
    );
    assert_eq!(wal.last_sequence(), 1);
    drop(wal);
    cleanup(&path);
}

#[test]
fn ownership_epoch_must_advance_and_expected_epoch_must_match() {
    let path = path();
    cleanup(&path);
    let mut tablet = WalBackedTablet::open(descriptor(5), &path).unwrap();
    assert_eq!(
        tablet.advance_ownership_epoch(5, 5).unwrap_err(),
        WalBackedError::Tablet(TabletError::EpochNotAdvanced {
            current: 5,
            proposed: 5,
        })
    );
    assert_eq!(
        tablet.advance_ownership_epoch(4, 6).unwrap_err(),
        WalBackedError::Tablet(TabletError::StaleEpoch {
            current: 5,
            presented: 4,
        })
    );
    assert_eq!(tablet.current_sequence(), 0);
    assert_eq!(tablet.advance_ownership_epoch(5, 6).unwrap(), 1);
    assert_eq!(tablet.current_sequence(), 1);
    drop(tablet);

    assert!(WalBackedTablet::open(descriptor(5), &path).is_err());
    {
        let wal = FileWal::open(&path).unwrap();
        assert_eq!(wal.last_sequence(), 1);
        assert_eq!(wal.latest_ownership_epoch(), Some(6));
        assert_eq!(wal.records().len(), 1);
        assert!(wal.records()[0].is_epoch_fence());
        assert!(wal.records()[0].mutations().is_empty());
    }
    let reopened = WalBackedTablet::open(descriptor(6), &path).unwrap();
    assert_eq!(reopened.current_sequence(), 1);
    drop(reopened);
    cleanup(&path);
}

#[test]
fn fence_after_checkpoint_replays_without_losing_existing_receipts() {
    let path = path();
    cleanup(&path);
    let id = TransactionId::new([53; 16]);
    let mut tablet = WalBackedTablet::open(descriptor(11), &path).unwrap();
    let receipt = tablet.commit_idempotent(id, put(b"r", b"receipt")).unwrap();
    tablet.checkpoint().unwrap();
    assert_eq!(tablet.advance_ownership_epoch(11, 12).unwrap(), 2);
    drop(tablet);

    assert!(WalBackedTablet::open(descriptor(11), &path).is_err());
    let mut reopened = WalBackedTablet::open(descriptor(12), &path).unwrap();
    assert_eq!(reopened.current_sequence(), 2);
    assert_eq!(reopened.commit_idempotent(id, put(b"r", b"receipt")).unwrap(), receipt);
    assert_eq!(reopened.current_sequence(), 2);
    assert_eq!(reopened.read_latest(b"r"), Some(&b"receipt"[..]));
    drop(reopened);
    cleanup(&path);
}
