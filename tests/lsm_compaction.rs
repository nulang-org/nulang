use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use nulang::database::lsm::LsmStorage;
use nulang::database::managed_lsm::{ManagedLsmError, ManagedLsmStorage};
use nulang::database::tablet::{
    KeyRange, MvccStorage, Tablet, TabletDescriptor, TabletError, TabletId, TabletMutation,
};

static NEXT_TEST: AtomicU64 = AtomicU64::new(1);

fn temp_dir(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "nulang_nudb_compaction_{name}_{}_{}",
        std::process::id(),
        NEXT_TEST.fetch_add(1, Ordering::Relaxed)
    ))
}

fn descriptor() -> TabletDescriptor {
    TabletDescriptor::new(
        TabletId::new(81).unwrap(),
        KeyRange::new(b"a".to_vec(), Some(b"z".to_vec())).unwrap(),
        1,
    )
    .unwrap()
}

fn put(key: &[u8], value: &[u8]) -> TabletMutation {
    TabletMutation::Put {
        key: key.to_vec(),
        value: value.to_vec(),
    }
}

#[test]
fn compaction_atomically_replaces_the_active_table_set_and_survives_restart() {
    let dir = temp_dir("restart");
    let _ = fs::remove_dir_all(&dir);

    {
        let mut storage = ManagedLsmStorage::open(&dir).unwrap();
        storage.apply_committed(1, vec![put(b"k", b"v1")]);
        storage.flush().unwrap().unwrap();
        storage.apply_committed(2, vec![put(b"k", b"v2")]);
        storage.flush().unwrap().unwrap();
        storage.apply_committed(3, vec![put(b"other", b"x")]);
        storage.flush().unwrap().unwrap();

        assert_eq!(storage.table_count(), 3);
        let compacted = storage.compact_all(2).unwrap();
        assert_eq!(compacted.input_tables, 3);
        assert_eq!(compacted.output_tables, 1);
        assert_eq!(compacted.versions_removed, 1);
        assert_eq!(storage.table_count(), 1);
        assert_eq!(storage.oldest_readable_sequence(), 2);
        assert_eq!(storage.read_at(b"k", 2), Some(&b"v2"[..]));
        assert_eq!(storage.read_at(b"other", 3), Some(&b"x"[..]));
        assert!(dir.join("MANIFEST").exists());
    }

    let reopened = ManagedLsmStorage::open(&dir).unwrap();
    assert_eq!(reopened.current_sequence(), 3);
    assert_eq!(reopened.oldest_readable_sequence(), 2);
    assert_eq!(reopened.table_count(), 1);
    assert_eq!(reopened.read_at(b"k", 2), Some(&b"v2"[..]));
    assert_eq!(reopened.read_at(b"other", 3), Some(&b"x"[..]));

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn compaction_reclaims_a_terminal_tombstone() {
    let dir = temp_dir("tombstone");
    let _ = fs::remove_dir_all(&dir);

    let mut storage = ManagedLsmStorage::open(&dir).unwrap();
    storage.apply_committed(1, vec![put(b"k", b"value")]);
    storage.flush().unwrap().unwrap();
    storage.apply_committed(2, vec![TabletMutation::Delete { key: b"k".to_vec() }]);
    storage.flush().unwrap().unwrap();

    let compacted = storage.compact_all(2).unwrap();
    assert_eq!(compacted.keys_removed, 1);
    assert_eq!(compacted.versions_removed, 2);
    assert_eq!(storage.read_at(b"k", 2), None);

    drop(storage);
    let reopened = ManagedLsmStorage::open(&dir).unwrap();
    assert_eq!(reopened.current_sequence(), 2);
    assert_eq!(reopened.oldest_readable_sequence(), 2);
    assert_eq!(reopened.read_at(b"k", 2), None);

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn manifest_ignores_an_unreferenced_sstable_from_an_interrupted_operation() {
    let dir = temp_dir("orphan");
    let _ = fs::remove_dir_all(&dir);

    {
        let mut storage = ManagedLsmStorage::open(&dir).unwrap();
        storage.apply_committed(1, vec![put(b"k", b"v1")]);
        storage.flush().unwrap().unwrap();
    }

    let orphan = dir.join("nudb-sst-00000000000000000099.sst");
    fs::write(&orphan, b"interrupted unpublished table").unwrap();

    let reopened = ManagedLsmStorage::open(&dir).unwrap();
    assert_eq!(reopened.current_sequence(), 1);
    assert_eq!(reopened.table_count(), 1);
    assert_eq!(reopened.read_at(b"k", 1), Some(&b"v1"[..]));

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn corrupt_manifest_fails_closed() {
    let dir = temp_dir("corrupt_manifest");
    let _ = fs::remove_dir_all(&dir);

    {
        let mut storage = ManagedLsmStorage::open(&dir).unwrap();
        storage.apply_committed(1, vec![put(b"k", b"v1")]);
        storage.flush().unwrap().unwrap();
    }

    let manifest = dir.join("MANIFEST");
    let mut bytes = fs::read(&manifest).unwrap();
    let index = bytes.len() / 2;
    bytes[index] ^= 0xff;
    fs::write(&manifest, bytes).unwrap();

    assert!(matches!(
        ManagedLsmStorage::open(&dir),
        Err(ManagedLsmError::ManifestChecksumMismatch { .. })
            | Err(ManagedLsmError::CorruptManifest { .. })
            | Err(ManagedLsmError::UnsupportedManifestVersion { .. })
    ));

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn compaction_rejects_unflushed_commits() {
    let dir = temp_dir("dirty_memtable");
    let _ = fs::remove_dir_all(&dir);

    let mut storage = ManagedLsmStorage::open(&dir).unwrap();
    storage.apply_committed(1, vec![put(b"k", b"v1")]);
    storage.flush().unwrap().unwrap();
    storage.apply_committed(2, vec![put(b"k", b"v2")]);

    assert_eq!(
        storage.compact_all(1).unwrap_err(),
        ManagedLsmError::UnflushedStateForCompaction {
            current_sequence: 2,
            flushed_sequence: 1,
        }
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn managed_storage_bootstraps_existing_sstable_directories() {
    let dir = temp_dir("legacy_bootstrap");
    let _ = fs::remove_dir_all(&dir);

    {
        let mut legacy = LsmStorage::open(&dir).unwrap();
        legacy.apply_committed(1, vec![put(b"k", b"v1")]);
        legacy.flush().unwrap().unwrap();
        legacy.apply_committed(2, vec![put(b"k", b"v2")]);
        legacy.flush().unwrap().unwrap();
    }

    assert!(!dir.join("MANIFEST").exists());
    let managed = ManagedLsmStorage::open(&dir).unwrap();
    assert_eq!(managed.current_sequence(), 2);
    assert_eq!(managed.table_count(), 2);
    assert_eq!(managed.read_at(b"k", 1), Some(&b"v1"[..]));
    assert_eq!(managed.read_at(b"k", 2), Some(&b"v2"[..]));
    assert!(dir.join("MANIFEST").exists());

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn tablet_rejects_snapshots_below_the_compacted_retention_floor() {
    let dir = temp_dir("retention_fence");
    let _ = fs::remove_dir_all(&dir);

    let mut storage = ManagedLsmStorage::open(&dir).unwrap();
    storage.apply_committed(1, vec![put(b"k", b"v1")]);
    storage.flush().unwrap().unwrap();
    storage.apply_committed(2, vec![put(b"k", b"v2")]);
    storage.flush().unwrap().unwrap();
    storage.compact_all(2).unwrap();

    let tablet = Tablet::with_storage(descriptor(), storage);
    assert_eq!(
        tablet.read_at(b"k", 1).unwrap_err(),
        TabletError::SnapshotCollected {
            oldest_readable: 2,
            requested: 1,
        }
    );
    assert_eq!(tablet.read_at(b"k", 2).unwrap(), Some(&b"v2"[..]));

    let _ = fs::remove_dir_all(&dir);
}
