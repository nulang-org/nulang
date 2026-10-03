use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use super::interruption::{with_interruption, StorageInterruptionPoint};
use super::store::{WalBackedError, WalBackedTablet};
use super::tablet::{KeyRange, TabletDescriptor, TabletId, TabletMutation};

static NEXT_TEST: AtomicU64 = AtomicU64::new(1);

fn temp_wal(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "nulang_nudb_compaction_interrupt_{name}_{}_{}.wal",
        std::process::id(),
        NEXT_TEST.fetch_add(1, Ordering::Relaxed)
    ))
}

fn descriptor() -> TabletDescriptor {
    TabletDescriptor::new(
        TabletId::new(906).unwrap(),
        KeyRange::new(b"a".to_vec(), Some(b"z".to_vec())).unwrap(),
        1,
    )
    .unwrap()
}

fn appended(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

fn cleanup(path: &Path) {
    let manifest = path.with_extension("manifest");
    let _ = fs::remove_file(path);
    let _ = fs::remove_file(path.with_extension("checkpoint"));
    let _ = fs::remove_file(&manifest);
    let _ = fs::remove_file(appended(&manifest, ".tmp"));
    let _ = fs::remove_dir_all(path.with_extension("sstables"));
}

fn commit_put(tablet: &mut WalBackedTablet, value: &[u8]) {
    let sequence = tablet.current_sequence();
    let write = tablet
        .prepare_write(
            1,
            sequence,
            vec![TabletMutation::Put {
                key: b"k".to_vec(),
                value: value.to_vec(),
            }],
        )
        .unwrap();
    tablet.commit(write).unwrap();
}

fn flush_current(tablet: &mut WalBackedTablet) {
    let bytes = tablet.mutable_memtable_bytes();
    assert!(bytes > 0);
    assert!(tablet.rotate_memtable_if_bytes_at_least(bytes));
    assert!(tablet.flush_oldest_immutable_to_sstable().unwrap());
}

fn build_four_tables(path: &Path) -> WalBackedTablet {
    let mut tablet = WalBackedTablet::open(descriptor(), path).unwrap();
    for value in [
        b"v1".as_slice(),
        b"v2".as_slice(),
        b"v3".as_slice(),
        b"v4".as_slice(),
    ] {
        commit_put(&mut tablet, value);
        flush_current(&mut tablet);
    }
    assert_eq!(tablet.durable_sstable_count().unwrap(), 4);
    tablet
}

fn sstable_paths(path: &Path) -> Vec<PathBuf> {
    let mut paths = fs::read_dir(path.with_extension("sstables"))
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| path.extension().is_some_and(|ext| ext == "sst"))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    paths.sort();
    paths
}

fn sstable_file_count(path: &Path) -> usize {
    sstable_paths(path).len()
}

fn assert_history(tablet: &WalBackedTablet) {
    for (snapshot, expected) in [
        (1, b"v1".as_slice()),
        (2, b"v2".as_slice()),
        (3, b"v3".as_slice()),
        (4, b"v4".as_slice()),
    ] {
        assert_eq!(
            tablet.read_at(b"k", snapshot).unwrap().as_deref(),
            Some(expected)
        );
    }
}

#[test]
fn compaction_interrupted_before_manifest_rename_keeps_sources_authoritative() {
    let path = temp_wal("before_manifest_rename");
    cleanup(&path);
    let mut tablet = build_four_tables(&path);

    let result = with_interruption(StorageInterruptionPoint::ManifestAfterTempSync, || {
        tablet.compact_l0_once()
    });
    assert!(result.is_err());
    drop(tablet);

    // The replacement SSTable is a harmless orphan because the old manifest
    // never stopped naming the four source tables.
    let mut reopened = WalBackedTablet::open(descriptor(), &path).unwrap();
    assert_eq!(reopened.durable_sstable_count().unwrap(), 4);
    assert_eq!(sstable_file_count(&path), 5);
    assert_history(&reopened);

    // Retrying deterministically reuses the already-durable replacement and
    // then retires the four sources.
    assert!(reopened.compact_l0_once().unwrap());
    assert_eq!(reopened.durable_sstable_count().unwrap(), 1);
    assert_eq!(sstable_file_count(&path), 1);
    assert_history(&reopened);
    cleanup(&path);
}

#[test]
fn compaction_interrupted_after_manifest_rename_recovers_replacement_authority() {
    let path = temp_wal("after_manifest_rename");
    cleanup(&path);
    let mut tablet = build_four_tables(&path);

    let result = with_interruption(StorageInterruptionPoint::ManifestAfterRename, || {
        tablet.compact_l0_once()
    });
    assert!(result.is_err());
    drop(tablet);

    // Rename is the manifest commit point. Reopen sees the replacement as
    // authoritative, validates the complete recovery chain, then consumes
    // persisted obsolete-file intent and clears it durably.
    let reopened = WalBackedTablet::open(descriptor(), &path).unwrap();
    assert_eq!(reopened.current_sequence(), 4);
    assert_eq!(reopened.durable_sstable_count().unwrap(), 1);
    assert_eq!(sstable_file_count(&path), 1);
    assert_history(&reopened);
    cleanup(&path);
}

#[test]
fn corrupt_committed_replacement_does_not_delete_source_fallbacks() {
    let path = temp_wal("corrupt_replacement");
    cleanup(&path);
    let mut tablet = build_four_tables(&path);
    let source_paths = sstable_paths(&path);
    assert_eq!(source_paths.len(), 4);

    let result = with_interruption(StorageInterruptionPoint::ManifestAfterRename, || {
        tablet.compact_l0_once()
    });
    assert!(result.is_err());
    drop(tablet);

    let all_paths = sstable_paths(&path);
    assert_eq!(all_paths.len(), 5);
    let replacement = all_paths
        .iter()
        .find(|candidate| !source_paths.contains(candidate))
        .expect("replacement SSTable must exist after manifest rename");
    let mut bytes = fs::read(replacement).unwrap();
    bytes[20] ^= 0x40;
    fs::write(replacement, bytes).unwrap();

    assert!(matches!(
        WalBackedTablet::open(descriptor(), &path),
        Err(WalBackedError::Sstable(_))
    ));
    assert_eq!(sstable_file_count(&path), 5);
    for source in &source_paths {
        assert!(
            source.exists(),
            "source SSTable was retired before replacement validation"
        );
    }

    cleanup(&path);
}
