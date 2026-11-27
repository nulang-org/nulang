use std::cell::Cell;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use super::store::{WalBackedError, WalBackedTablet};
use super::tablet::{KeyRange, TabletDescriptor, TabletError, TabletId, TabletMutation};
use super::wal::{FileWal, WalError};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StorageInterruptionPoint {
    WalAfterHeader,
    WalAfterPayload,
    WalAfterChecksum,
    WalAfterSync,
    WalGroupAfterFirstFrame,
    WalGroupAfterSecondHeader,
    WalGroupAfterSync,
    CheckpointAfterTempWrite,
    CheckpointAfterTempSync,
    CheckpointAfterRename,
    CheckpointAfterDirectorySync,
    WalReclaimAfterReplacementWrite,
    WalReclaimAfterReplacementSync,
    WalReclaimAfterRename,
    WalReclaimAfterDirectorySync,
    BackupBeforeCompletion,
    QuarantineBeforeRename,
    QuarantineAfterRename,
}

thread_local! {
    static ACTIVE: Cell<Option<StorageInterruptionPoint>> = const { Cell::new(None) };
}

struct ResetInterruption;

impl Drop for ResetInterruption {
    fn drop(&mut self) {
        ACTIVE.with(|active| active.set(None));
    }
}

pub(crate) fn with_interruption<T>(
    point: StorageInterruptionPoint,
    operation: impl FnOnce() -> T,
) -> T {
    ACTIVE.with(|active| {
        assert!(
            active.get().is_none(),
            "nested storage interruption is unsupported"
        );
        active.set(Some(point));
    });
    let _reset = ResetInterruption;
    operation()
}

pub(crate) fn hit(point: StorageInterruptionPoint) -> io::Result<()> {
    let should_interrupt = ACTIVE.with(|active| active.get() == Some(point));
    if should_interrupt {
        return Err(io::Error::other(format!(
            "injected NulangDB storage interruption at {point:?}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    static NEXT_TEST: AtomicU64 = AtomicU64::new(1);

    fn temp_wal(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "nulang_nudb_interrupt_{name}_{}_{}.wal",
            std::process::id(),
            NEXT_TEST.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn descriptor() -> TabletDescriptor {
        TabletDescriptor::new(
            TabletId::new(97).unwrap(),
            KeyRange::new(b"a".to_vec(), Some(b"z".to_vec())).unwrap(),
            7,
        )
        .unwrap()
    }

    fn appended(path: &Path, suffix: &str) -> PathBuf {
        let mut value = path.as_os_str().to_os_string();
        value.push(suffix);
        PathBuf::from(value)
    }

    fn cleanup(wal_path: &Path) {
        let checkpoint = wal_path.with_extension("checkpoint");
        let _ = fs::remove_file(wal_path);
        let _ = fs::remove_file(&checkpoint);
        let _ = fs::remove_file(appended(&checkpoint, ".tmp"));
        let _ = fs::remove_file(appended(wal_path, ".reclaim.tmp"));
    }

    fn commit_put(tablet: &mut WalBackedTablet, value: &[u8]) {
        let sequence = tablet.current_sequence();
        let write = tablet
            .prepare_write(
                7,
                sequence,
                vec![TabletMutation::Put {
                    key: b"k".to_vec(),
                    value: value.to_vec(),
                }],
            )
            .unwrap();
        tablet.commit(write).unwrap();
    }

    #[test]
    fn grouped_writes_remain_replayable_after_pre_sync_and_post_sync_failures() {
        use super::super::tablet::TransactionId;

        for (point, recovered_sequence) in [
            (StorageInterruptionPoint::WalGroupAfterFirstFrame, 1),
            (StorageInterruptionPoint::WalGroupAfterSecondHeader, 1),
            (StorageInterruptionPoint::WalGroupAfterSync, 3),
        ] {
            let path = temp_wal("group");
            cleanup(&path);
            let entries = vec![
                (
                    TransactionId::new([1; 16]),
                    vec![TabletMutation::Put {
                        key: b"k".to_vec(),
                        value: b"one".to_vec(),
                    }],
                ),
                (
                    TransactionId::new([2; 16]),
                    vec![TabletMutation::Put {
                        key: b"k".to_vec(),
                        value: b"two".to_vec(),
                    }],
                ),
                (
                    TransactionId::new([3; 16]),
                    vec![TabletMutation::Put {
                        key: b"k".to_vec(),
                        value: b"three".to_vec(),
                    }],
                ),
            ];

            let mut tablet = WalBackedTablet::open(descriptor(), &path).unwrap();
            let result =
                with_interruption(point, || tablet.commit_batch_idempotent(entries.clone()));
            assert!(result.is_err(), "{point:?} must interrupt the group");
            assert_eq!(
                tablet.current_sequence(),
                0,
                "an ambiguous group must not publish any MVCC record before acknowledgement"
            );
            let attempted = tablet.commit_batch_idempotent(vec![(
                TransactionId::new([4; 16]),
                vec![TabletMutation::Put {
                    key: b"k".to_vec(),
                    value: b"unsafe".to_vec(),
                }],
            )]);
            assert_eq!(
                attempted.unwrap_err(),
                WalBackedError::Wal(WalError::Poisoned)
            );
            assert_eq!(
                tablet.commit_batch_idempotent(vec![]).unwrap_err(),
                WalBackedError::Wal(WalError::Poisoned)
            );
            drop(tablet);

            let mut reopened = WalBackedTablet::open(descriptor(), &path).unwrap();
            assert_eq!(
                reopened.current_sequence(),
                recovered_sequence,
                "recovery must select a valid transaction prefix after {point:?}"
            );
            let receipts = reopened.commit_batch_idempotent(entries).unwrap();
            assert_eq!(
                receipts
                    .iter()
                    .map(|receipt| receipt.sequence())
                    .collect::<Vec<_>>(),
                vec![1, 2, 3],
                "retries must reconcile the durable prefix without duplicates"
            );
            assert_eq!(reopened.read_latest(b"k"), Some(&b"three"[..]));
            drop(reopened);
            cleanup(&path);
            let _ = fs::remove_file(appended(&path, ".lock"));
        }
    }

    #[test]
    fn wal_append_interruption_matrix_recovers_a_valid_prefix_and_can_continue() {
        for (point, expected_sequence) in [
            (StorageInterruptionPoint::WalAfterHeader, 0),
            (StorageInterruptionPoint::WalAfterPayload, 0),
            (StorageInterruptionPoint::WalAfterChecksum, 1),
            (StorageInterruptionPoint::WalAfterSync, 1),
        ] {
            let wal_path = temp_wal("wal");
            cleanup(&wal_path);

            let mut tablet = WalBackedTablet::open(descriptor(), &wal_path).unwrap();
            let write = tablet
                .prepare_write(
                    7,
                    0,
                    vec![TabletMutation::Put {
                        key: b"k".to_vec(),
                        value: b"v1".to_vec(),
                    }],
                )
                .unwrap();

            let result = with_interruption(point, || tablet.commit(write));
            assert!(result.is_err(), "{point:?} must interrupt the commit");
            drop(tablet);

            let mut reopened = WalBackedTablet::open(descriptor(), &wal_path).unwrap();
            assert_eq!(
                reopened.current_sequence(),
                expected_sequence,
                "unexpected recovered sequence after {point:?}"
            );
            assert_eq!(
                reopened.read_latest(b"k"),
                (expected_sequence == 1).then_some(&b"v1"[..])
            );

            commit_put(&mut reopened, b"next");
            assert_eq!(reopened.current_sequence(), expected_sequence + 1);
            cleanup(&wal_path);
        }
    }

    #[test]
    fn checkpoint_publication_interruption_matrix_keeps_wal_recovery_safe() {
        for (point, checkpoint_published) in [
            (StorageInterruptionPoint::CheckpointAfterTempWrite, false),
            (StorageInterruptionPoint::CheckpointAfterTempSync, false),
            (StorageInterruptionPoint::CheckpointAfterRename, true),
            (StorageInterruptionPoint::CheckpointAfterDirectorySync, true),
        ] {
            let wal_path = temp_wal("checkpoint");
            cleanup(&wal_path);

            let mut tablet = WalBackedTablet::open(descriptor(), &wal_path).unwrap();
            commit_put(&mut tablet, b"v1");
            commit_put(&mut tablet, b"v2");

            let result = with_interruption(point, || tablet.publish_checkpoint());
            assert!(
                result.is_err(),
                "{point:?} must interrupt checkpoint publication"
            );
            drop(tablet);

            assert_eq!(
                wal_path.with_extension("checkpoint").exists(),
                checkpoint_published,
                "unexpected canonical checkpoint state after {point:?}"
            );

            let reopened = WalBackedTablet::open(descriptor(), &wal_path).unwrap();
            assert_eq!(reopened.current_sequence(), 2);
            assert_eq!(reopened.read_at(b"k", 1).unwrap(), Some(&b"v1"[..]));
            assert_eq!(reopened.read_at(b"k", 2).unwrap(), Some(&b"v2"[..]));
            cleanup(&wal_path);
        }
    }

    #[test]
    fn wal_reclamation_interruption_matrix_preserves_checkpoint_and_sequence_chain() {
        for (point, expected_base_sequence, expected_records) in [
            (
                StorageInterruptionPoint::WalReclaimAfterReplacementWrite,
                0,
                2,
            ),
            (
                StorageInterruptionPoint::WalReclaimAfterReplacementSync,
                0,
                2,
            ),
            (StorageInterruptionPoint::WalReclaimAfterRename, 2, 0),
            (StorageInterruptionPoint::WalReclaimAfterDirectorySync, 2, 0),
        ] {
            let wal_path = temp_wal("reclaim");
            cleanup(&wal_path);

            let mut tablet = WalBackedTablet::open(descriptor(), &wal_path).unwrap();
            commit_put(&mut tablet, b"v1");
            commit_put(&mut tablet, b"v2");

            let result = with_interruption(point, || tablet.checkpoint());
            assert!(result.is_err(), "{point:?} must interrupt WAL reclamation");

            // Once reclamation starts, any error must make the live WAL
            // unusable. After rename, self.file can still refer to the old
            // unlinked inode while the canonical path names the replacement;
            // accepting another commit in that state could acknowledge data
            // that disappears on restart.
            let sequence = tablet.current_sequence();
            let retry = tablet
                .prepare_write(
                    7,
                    sequence,
                    vec![TabletMutation::Put {
                        key: b"k".to_vec(),
                        value: b"must-not-commit".to_vec(),
                    }],
                )
                .unwrap();
            assert_eq!(
                tablet.commit(retry).unwrap_err(),
                WalBackedError::Wal(WalError::Poisoned),
                "live tablet must require reopen after {point:?}"
            );
            drop(tablet);

            assert!(
                wal_path.with_extension("checkpoint").exists(),
                "checkpoint must already be durable before WAL reclamation"
            );

            let wal = FileWal::open(&wal_path).unwrap();
            assert_eq!(wal.base_sequence(), expected_base_sequence);
            assert_eq!(wal.records().len(), expected_records);
            drop(wal);

            let mut reopened = WalBackedTablet::open(descriptor(), &wal_path).unwrap();
            assert_eq!(reopened.current_sequence(), 2);
            assert_eq!(reopened.read_at(b"k", 1).unwrap(), Some(&b"v1"[..]));
            assert_eq!(reopened.read_at(b"k", 2).unwrap(), Some(&b"v2"[..]));

            commit_put(&mut reopened, b"v3");
            assert_eq!(reopened.current_sequence(), 3);
            cleanup(&wal_path);
        }
    }

    #[test]
    fn mvcc_retention_checkpoint_failures_require_reopen_before_further_writes() {
        for (point, persisted_floor) in [
            (StorageInterruptionPoint::CheckpointAfterTempWrite, 0),
            (StorageInterruptionPoint::CheckpointAfterTempSync, 0),
            (StorageInterruptionPoint::CheckpointAfterRename, 2),
            (StorageInterruptionPoint::CheckpointAfterDirectorySync, 2),
        ] {
            let wal_path = temp_wal("retention_checkpoint");
            cleanup(&wal_path);

            let mut tablet = WalBackedTablet::open(descriptor(), &wal_path).unwrap();
            commit_put(&mut tablet, b"v1");
            commit_put(&mut tablet, b"v2");
            assert!(with_interruption(point, || tablet.reclaim_history_before(2)).is_err());

            // The checkpoint may have been renamed to its canonical location
            // even though the caller observed an error. Until recovery, the
            // live tablet cannot assume that its snapshot floor matches disk.
            assert_eq!(
                tablet.read_at(b"k", 1),
                Err(TabletError::RecoveryRequired),
                "{point:?} must invalidate snapshot reads on the live handle"
            );
            assert_eq!(
                tablet.prepare_write(
                    7,
                    tablet.current_sequence(),
                    vec![TabletMutation::Put {
                        key: b"k".to_vec(),
                        value: b"unsafe".to_vec(),
                    }],
                ),
                Err(TabletError::RecoveryRequired),
                "{point:?} must prohibit live writes"
            );
            assert!(
                tablet.publish_checkpoint().is_err(),
                "{point:?} must not overwrite a possibly newer retention checkpoint"
            );
            drop(tablet);

            let mut reopened = WalBackedTablet::open(descriptor(), &wal_path).unwrap();
            assert_eq!(reopened.oldest_retained_snapshot(), persisted_floor);
            assert_eq!(reopened.read_latest(b"k"), Some(&b"v2"[..]));
            if persisted_floor == 2 {
                assert_eq!(
                    reopened.read_at(b"k", 1),
                    Err(TabletError::SnapshotExpired {
                        requested: 1,
                        oldest_retained: 2,
                    })
                );
            } else {
                assert_eq!(reopened.read_at(b"k", 1).unwrap(), Some(&b"v1"[..]));
            }
            commit_put(&mut reopened, b"v3");
            assert_eq!(reopened.read_latest(b"k"), Some(&b"v3"[..]));
            cleanup(&wal_path);
        }
    }

    #[test]
    fn mvcc_retention_wal_reclaim_failures_preserve_floor_and_sequence() {
        for point in [
            StorageInterruptionPoint::WalReclaimAfterReplacementWrite,
            StorageInterruptionPoint::WalReclaimAfterReplacementSync,
            StorageInterruptionPoint::WalReclaimAfterRename,
            StorageInterruptionPoint::WalReclaimAfterDirectorySync,
        ] {
            let wal_path = temp_wal("retention_reclaim");
            cleanup(&wal_path);

            let mut tablet = WalBackedTablet::open(descriptor(), &wal_path).unwrap();
            commit_put(&mut tablet, b"v1");
            commit_put(&mut tablet, b"v2");
            assert!(with_interruption(point, || tablet.reclaim_history_before(2)).is_err());

            assert_eq!(tablet.read_at(b"k", 1), Err(TabletError::RecoveryRequired));
            assert!(tablet.publish_checkpoint().is_err());
            drop(tablet);

            let mut reopened = WalBackedTablet::open(descriptor(), &wal_path).unwrap();
            assert_eq!(reopened.current_sequence(), 2);
            assert_eq!(reopened.oldest_retained_snapshot(), 2);
            assert_eq!(
                reopened.read_at(b"k", 1),
                Err(TabletError::SnapshotExpired {
                    requested: 1,
                    oldest_retained: 2,
                })
            );
            assert_eq!(reopened.read_at(b"k", 2).unwrap(), Some(&b"v2"[..]));
            commit_put(&mut reopened, b"v3");
            assert_eq!(reopened.current_sequence(), 3);
            cleanup(&wal_path);
        }
    }
}

#[cfg(all(test, unix))]
#[test]
fn bound_reclaim_fault_matrix_recovers_old_or_bound_wal() {
    use super::sst_manifest::ManifestStore;
    use super::tablet::TransactionId;
    static NEXT: AtomicU64 = AtomicU64::new(0);
    for point in [
        StorageInterruptionPoint::WalReclaimAfterReplacementWrite,
        StorageInterruptionPoint::WalReclaimAfterReplacementSync,
        StorageInterruptionPoint::WalReclaimAfterRename,
        StorageInterruptionPoint::WalReclaimAfterDirectorySync,
    ] {
        let dir = std::env::temp_dir().join(format!(
            "nudb-bound-fault-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&dir).unwrap();
        fs::create_dir(dir.join("sst")).unwrap();
        let path = dir.join("tablet.wal");
        let desc = TabletDescriptor::new(
            TabletId::new(97).unwrap(),
            KeyRange::new(b"a".to_vec(), Some(b"z".to_vec())).unwrap(),
            7,
        )
        .unwrap();
        let mut db = WalBackedTablet::open(desc.clone(), &path).unwrap();
        let mutations = vec![TabletMutation::Put {
            key: b"k".to_vec(),
            value: b"value".to_vec(),
        }];
        let id = TransactionId::new([1; 16]);
        let receipt = db.commit_idempotent(id, mutations.clone()).unwrap();
        let mut manifest = ManifestStore::create(dir.join("sst"), 97, 7).unwrap();
        db.flush_sst_shadow(&mut manifest, 1).unwrap();
        db.seal_sst_recovery(&mut manifest).unwrap();
        let original = fs::read(&path).unwrap();
        let result = with_interruption(point, || db.reclaim_to_sealed_sst(&manifest));
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("injected NulangDB storage interruption"));
        assert!(db.commit_idempotent(id, mutations.clone()).is_err());
        assert!(db.read_at(b"k", 1).is_err());
        assert!(db.publish_checkpoint().is_err());
        drop(db);
        drop(manifest);
        let bytes = fs::read(&path).unwrap();
        if matches!(
            point,
            StorageInterruptionPoint::WalReclaimAfterReplacementWrite
                | StorageInterruptionPoint::WalReclaimAfterReplacementSync
        ) {
            assert_eq!(bytes, original);
        } else {
            assert_eq!(&bytes[..8], b"NUDBWAL4");
            assert_eq!(bytes.len(), 96);
        }
        let mut db =
            WalBackedTablet::open_from_sealed_sst(desc.clone(), &path, dir.join("sst")).unwrap();
        assert_eq!(db.read_at(b"k", 1).unwrap(), Some(b"value".as_slice()));
        assert_eq!(
            db.commit_idempotent(id, mutations.clone()).unwrap(),
            receipt
        );
        assert_eq!(
            db.commit_idempotent(TransactionId::new([2; 16]), vec![])
                .unwrap()
                .sequence(),
            2
        );
        drop(db);
        let db = WalBackedTablet::open_from_sealed_sst(desc, &path, dir.join("sst")).unwrap();
        assert_eq!(db.current_sequence(), 2);
        drop(db);
        fs::remove_dir_all(dir).unwrap();
    }
}

#[cfg(all(test, unix))]
#[test]
fn nonzero_seal_floor_cutover_is_atomic_with_wal_binding() {
    use super::sst_manifest::ManifestStore;
    use super::tablet::TransactionId;
    static NEXT: AtomicU64 = AtomicU64::new(0);
    for point in [
        StorageInterruptionPoint::WalReclaimAfterReplacementWrite,
        StorageInterruptionPoint::WalReclaimAfterReplacementSync,
        StorageInterruptionPoint::WalReclaimAfterRename,
        StorageInterruptionPoint::WalReclaimAfterDirectorySync,
    ] {
        let dir = std::env::temp_dir().join(format!(
            "nudb-floor-fault-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&dir).unwrap();
        fs::create_dir(dir.join("sst")).unwrap();
        let path = dir.join("tablet.wal");
        let desc = TabletDescriptor::new(
            TabletId::new(97).unwrap(),
            KeyRange::new(b"a".to_vec(), Some(b"z".to_vec())).unwrap(),
            7,
        )
        .unwrap();
        let mut db = WalBackedTablet::open(desc.clone(), &path).unwrap();
        let old = vec![TabletMutation::Put {
            key: b"k".to_vec(),
            value: b"old".to_vec(),
        }];
        let id = TransactionId::new([1; 16]);
        let receipt = db.commit_idempotent(id, old.clone()).unwrap();
        db.commit_idempotent(
            TransactionId::new([2; 16]),
            vec![TabletMutation::Put {
                key: b"k".to_vec(),
                value: b"anchor".to_vec(),
            }],
        )
        .unwrap();
        let mut m = ManifestStore::create(dir.join("sst"), 97, 7).unwrap();
        db.flush_sst_shadow(&mut m, 2).unwrap();
        db.seal_sst_recovery_at_floor(&mut m, 2).unwrap();
        let result = with_interruption(point, || db.reclaim_to_sealed_sst(&m));
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("injected NulangDB storage interruption"));
        assert!(db.read_at(b"k", 1).is_err());
        assert!(db.commit_idempotent(id, old.clone()).is_err());
        drop(db);
        drop(m);
        let before_rename = matches!(
            point,
            StorageInterruptionPoint::WalReclaimAfterReplacementWrite
                | StorageInterruptionPoint::WalReclaimAfterReplacementSync
        );
        let mut db = if before_rename {
            assert!(
                WalBackedTablet::open_from_sealed_sst(desc.clone(), &path, dir.join("sst"))
                    .is_err()
            );
            let db = WalBackedTablet::open(desc.clone(), &path).unwrap();
            assert_eq!(db.oldest_retained_snapshot(), 0);
            assert_eq!(db.read_at(b"k", 1).unwrap(), Some(b"old".as_slice()));
            db
        } else {
            assert!(WalBackedTablet::open(desc.clone(), &path).is_err());
            let db = WalBackedTablet::open_from_sealed_sst(desc.clone(), &path, dir.join("sst"))
                .unwrap();
            assert_eq!(db.oldest_retained_snapshot(), 2);
            assert!(db.read_at(b"k", 1).is_err());
            db
        };
        assert_eq!(db.read_at(b"k", 2).unwrap(), Some(b"anchor".as_slice()));
        assert_eq!(db.commit_idempotent(id, old).unwrap(), receipt);
        assert_eq!(
            db.commit_idempotent(TransactionId::new([3; 16]), vec![])
                .unwrap()
                .sequence(),
            3
        );
        drop(db);
        fs::remove_dir_all(dir).unwrap();
    }
}

#[cfg(all(test, unix))]
#[test]
fn successor_rotation_interruption_selects_exact_bound_generation() {
    use super::sst_manifest::ManifestStore;
    use super::tablet::TransactionId;
    static NEXT: AtomicU64 = AtomicU64::new(0);
    for point in [
        StorageInterruptionPoint::WalReclaimAfterReplacementWrite,
        StorageInterruptionPoint::WalReclaimAfterReplacementSync,
        StorageInterruptionPoint::WalReclaimAfterRename,
        StorageInterruptionPoint::WalReclaimAfterDirectorySync,
    ] {
        let dir = std::env::temp_dir().join(format!(
            "nudb-successor-fault-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&dir).unwrap();
        let old_dir = dir.join("old");
        let new_dir = dir.join("new");
        fs::create_dir(&old_dir).unwrap();
        fs::create_dir(&new_dir).unwrap();
        let path = dir.join("tablet.wal");
        let desc = TabletDescriptor::new(
            TabletId::new(97).unwrap(),
            KeyRange::new(b"a".to_vec(), Some(b"z".to_vec())).unwrap(),
            7,
        )
        .unwrap();
        let mut db = WalBackedTablet::open(desc.clone(), &path).unwrap();
        let first = vec![TabletMutation::Put {
            key: b"k".to_vec(),
            value: b"first".to_vec(),
        }];
        let receipt = db
            .commit_idempotent(TransactionId::new([1; 16]), first.clone())
            .unwrap();
        let mut old = ManifestStore::create(&old_dir, 97, 7).unwrap();
        db.flush_sst_shadow(&mut old, 1).unwrap();
        db.seal_sst_recovery_at_floor(&mut old, 1).unwrap();
        db.reclaim_to_sealed_sst(&old).unwrap();
        db.commit_idempotent(
            TransactionId::new([2; 16]),
            vec![TabletMutation::Put {
                key: b"k".to_vec(),
                value: b"tail".to_vec(),
            }],
        )
        .unwrap();
        let mut next = ManifestStore::create(&new_dir, 97, 7).unwrap();
        db.prepare_sst_successor(&old, &mut next, 2).unwrap();
        let before = fs::read(&path).unwrap();
        let old_manifest = fs::read(old_dir.join("MANIFEST")).unwrap();
        let result = with_interruption(point, || db.reclaim_to_sst_successor(&old, &next));
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("injected NulangDB storage interruption"));
        assert!(db
            .commit_idempotent(TransactionId::new([3; 16]), vec![])
            .is_err());
        assert!(db.read_at(b"k", 2).is_err());
        drop(db);
        drop(old);
        drop(next);
        assert_eq!(fs::read(old_dir.join("MANIFEST")).unwrap(), old_manifest);
        let pre_rename = matches!(
            point,
            StorageInterruptionPoint::WalReclaimAfterReplacementWrite
                | StorageInterruptionPoint::WalReclaimAfterReplacementSync
        );
        let (right, wrong, floor) = if pre_rename {
            assert_eq!(fs::read(&path).unwrap(), before);
            (&old_dir, &new_dir, 1)
        } else {
            assert_eq!(fs::metadata(&path).unwrap().len(), 96);
            (&new_dir, &old_dir, 2)
        };
        let bytes = fs::read(&path).unwrap();
        assert!(WalBackedTablet::open_from_sealed_sst(desc.clone(), &path, wrong).is_err());
        assert_eq!(fs::read(&path).unwrap(), bytes);
        let mut db = WalBackedTablet::open_from_sealed_sst(desc.clone(), &path, right).unwrap();
        assert_eq!(db.oldest_retained_snapshot(), floor);
        assert_eq!(db.current_sequence(), 2);
        assert_eq!(db.read_at(b"k", 2).unwrap(), Some(b"tail".as_slice()));
        assert_eq!(
            db.commit_idempotent(TransactionId::new([1; 16]), first)
                .unwrap(),
            receipt
        );
        assert_eq!(
            db.commit_idempotent(TransactionId::new([3; 16]), vec![])
                .unwrap()
                .sequence(),
            3
        );
        drop(db);
        let db = WalBackedTablet::open_from_sealed_sst(desc, &path, right).unwrap();
        assert_eq!(db.current_sequence(), 3);
        drop(db);
        fs::remove_dir_all(dir).unwrap();
    }
}

#[cfg(all(test, unix))]
#[test]
fn interrupted_backup_never_publishes_completion_and_live_wal_remains_writable() {
    use super::sst_manifest::ManifestStore;
    use super::tablet::TransactionId;
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "nudb-backup-fault-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&dir).unwrap();
    let sst = dir.join("sst");
    fs::create_dir(&sst).unwrap();
    let path = dir.join("tablet.wal");
    let desc = TabletDescriptor::new(
        TabletId::new(97).unwrap(),
        KeyRange::new(b"a".to_vec(), Some(b"z".to_vec())).unwrap(),
        7,
    )
    .unwrap();
    let mut db = WalBackedTablet::open(desc.clone(), &path).unwrap();
    db.commit_idempotent(TransactionId::new([1; 16]), vec![])
        .unwrap();
    let mut m = ManifestStore::create(&sst, 97, 7).unwrap();
    db.flush_sst_shadow(&mut m, 1).unwrap();
    db.seal_sst_recovery(&mut m).unwrap();
    db.reclaim_to_sealed_sst(&m).unwrap();
    db.commit_idempotent(TransactionId::new([2; 16]), vec![])
        .unwrap();
    let before = fs::read(&path).unwrap();
    let backup = dir.join("backup");
    let result = with_interruption(StorageInterruptionPoint::BackupBeforeCompletion, || {
        db.backup_sealed(&m, &backup)
    });
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("injected NulangDB storage interruption"));
    assert!(!backup.join("COMPLETE").exists());
    assert_eq!(fs::read(&path).unwrap(), before);
    assert!(WalBackedTablet::restore_sealed_backup(desc, &backup, dir.join("restore")).is_err());
    assert_eq!(
        db.commit_idempotent(TransactionId::new([3; 16]), vec![])
            .unwrap()
            .sequence(),
        3
    );
    drop(db);
    drop(m);
    fs::remove_dir_all(dir).unwrap();
}

#[cfg(all(test, unix))]
#[test]
fn interrupted_quarantine_preserves_both_active_recovery_and_obsolete_seal() {
    use super::sst_manifest::ManifestStore;
    use super::tablet::TransactionId;
    static NEXT: AtomicU64 = AtomicU64::new(0);
    for point in [
        StorageInterruptionPoint::QuarantineBeforeRename,
        StorageInterruptionPoint::QuarantineAfterRename,
    ] {
        let dir = std::env::temp_dir().join(format!(
            "nudb-quarantine-fault-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&dir).unwrap();
        let old_dir = dir.join("old");
        let new_dir = dir.join("new");
        fs::create_dir(&old_dir).unwrap();
        fs::create_dir(&new_dir).unwrap();
        let path = dir.join("tablet.wal");
        let desc = TabletDescriptor::new(
            TabletId::new(97).unwrap(),
            KeyRange::new(b"a".to_vec(), Some(b"z".to_vec())).unwrap(),
            7,
        )
        .unwrap();
        let mut db = WalBackedTablet::open(desc.clone(), &path).unwrap();
        db.commit_idempotent(TransactionId::new([1; 16]), vec![])
            .unwrap();
        let mut old = ManifestStore::create(&old_dir, 97, 7).unwrap();
        db.flush_sst_shadow(&mut old, 1).unwrap();
        db.seal_sst_recovery(&mut old).unwrap();
        db.reclaim_to_sealed_sst(&old).unwrap();
        db.commit_idempotent(TransactionId::new([2; 16]), vec![])
            .unwrap();
        let mut new = ManifestStore::create(&new_dir, 97, 7).unwrap();
        db.prepare_sst_successor(&old, &mut new, 1).unwrap();
        db.reclaim_to_sst_successor(&old, &new).unwrap();
        let backup = dir.join("backup");
        db.backup_sealed(&new, &backup).unwrap();
        let target = dir.join("quarantine");
        let before = fs::read(&path).unwrap();
        let result = with_interruption(point, || {
            db.quarantine_obsolete_sst(&new, old, &backup, &target)
        });
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("injected NulangDB storage interruption"));
        assert_eq!(fs::read(&path).unwrap(), before);
        let retained = if point == StorageInterruptionPoint::QuarantineBeforeRename {
            assert!(!target.exists());
            &old_dir
        } else {
            assert!(!old_dir.exists());
            &target
        };
        assert_eq!(
            ManifestStore::open(retained, 97, 7)
                .unwrap()
                .current()
                .durable_sequence(),
            1
        );
        assert_eq!(
            db.commit_idempotent(TransactionId::new([3; 16]), vec![])
                .unwrap()
                .sequence(),
            3
        );
        drop(db);
        drop(new);
        let db = WalBackedTablet::open_from_sealed_sst(desc.clone(), &path, &new_dir).unwrap();
        assert_eq!(db.current_sequence(), 3);
        let restored =
            WalBackedTablet::restore_sealed_backup(desc, &backup, dir.join("restored")).unwrap();
        assert_eq!(restored.current_sequence(), 2);
        drop(db);
        drop(restored);
        fs::remove_dir_all(dir).unwrap();
    }
}
