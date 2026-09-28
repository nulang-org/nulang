use std::cell::Cell;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use super::store::WalBackedTablet;
use super::tablet::{KeyRange, TabletDescriptor, TabletId, TabletMutation};
use super::wal::FileWal;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StorageInterruptionPoint {
    WalAfterHeader,
    WalAfterPayload,
    WalAfterChecksum,
    WalAfterSync,
    CheckpointAfterTempWrite,
    CheckpointAfterTempSync,
    CheckpointAfterRename,
    CheckpointAfterDirectorySync,
    WalReclaimAfterReplacementWrite,
    WalReclaimAfterReplacementSync,
    WalReclaimAfterRename,
    WalReclaimAfterDirectorySync,
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
}
