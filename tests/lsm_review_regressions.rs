use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use nulang::database::lsm::{LsmError, LsmStorage};
use nulang::database::managed_lsm::{ManagedLsmError, ManagedLsmStorage};
use nulang::database::tablet::{MvccStorage, TabletMutation};

static NEXT_TEST: AtomicU64 = AtomicU64::new(1);

fn temp_dir(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "nulang_nudb_review_{name}_{}_{}",
        std::process::id(),
        NEXT_TEST.fetch_add(1, Ordering::Relaxed)
    ))
}

fn put(key: &[u8], value: &[u8]) -> TabletMutation {
    TabletMutation::Put {
        key: key.to_vec(),
        value: value.to_vec(),
    }
}

fn write_table(path: &Path, key_count: u64, declared_versions: u64, payload: &[u8]) {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"NUDBSST1");
    bytes.extend_from_slice(&1_u16.to_le_bytes());
    bytes.extend_from_slice(&1_u64.to_le_bytes());
    bytes.extend_from_slice(&1_u64.to_le_bytes());
    bytes.extend_from_slice(&1_u64.to_le_bytes());
    bytes.extend_from_slice(&key_count.to_le_bytes());
    bytes.extend_from_slice(&declared_versions.to_le_bytes());
    bytes.extend_from_slice(payload);
    let checksum = blake3::hash(&bytes);
    bytes.extend_from_slice(checksum.as_bytes());
    fs::write(path, bytes).unwrap();
}

fn one_tombstone_row(row_version_count: u32) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&1_u32.to_le_bytes());
    payload.push(b'k');
    payload.extend_from_slice(&row_version_count.to_le_bytes());
    payload.extend_from_slice(&1_u64.to_le_bytes());
    payload.push(0);
    payload
}

#[test]
fn sstable_rejects_impossible_key_count_before_allocating_rows() {
    let dir = temp_dir("bad_key_count");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let table = dir.join("nudb-sst-00000000000000000001.sst");
    write_table(&table, 2, 1, &one_tombstone_row(1));

    assert!(matches!(
        LsmStorage::open(&dir),
        Err(LsmError::CorruptTable { reason, .. })
            if reason == "key count exceeds minimum payload capacity"
    ));

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn sstable_rejects_impossible_row_version_count_before_allocating_versions() {
    let dir = temp_dir("bad_version_count");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let table = dir.join("nudb-sst-00000000000000000001.sst");
    write_table(&table, 1, 4, &one_tombstone_row(4));

    assert!(matches!(
        LsmStorage::open(&dir),
        Err(LsmError::CorruptTable { reason, .. })
            if reason == "row version count exceeds remaining payload capacity"
    ));

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn checked_point_reads_enforce_the_same_snapshot_fence_as_scans() {
    let dir = temp_dir("checked_read");
    let _ = fs::remove_dir_all(&dir);

    let mut storage = ManagedLsmStorage::open(&dir).unwrap();
    storage.apply_committed(1, vec![put(b"k", b"v1")]);
    storage.flush().unwrap().unwrap();
    storage.apply_committed(2, vec![put(b"k", b"v2")]);
    storage.flush().unwrap().unwrap();
    storage.compact_all(2).unwrap();

    assert_eq!(
        storage.try_read_at(b"k", 1).unwrap_err(),
        ManagedLsmError::SnapshotCollected {
            oldest_readable: 2,
            requested: 1,
        }
    );
    assert_eq!(storage.try_read_at(b"k", 2).unwrap(), Some(&b"v2"[..]));
    assert_eq!(
        storage.try_read_at(b"k", 3).unwrap_err(),
        ManagedLsmError::SnapshotAhead {
            committed: 2,
            requested: 3,
        }
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn limited_scan_counts_live_rows_not_tombstoned_candidates() {
    let dir = temp_dir("scan_limit_tombstone");
    let _ = fs::remove_dir_all(&dir);

    let mut storage = ManagedLsmStorage::open(&dir).unwrap();
    storage.apply_committed(1, vec![put(b"a", b"a1"), put(b"b", b"b1"), put(b"c", b"c1")]);
    storage.flush().unwrap().unwrap();
    storage.apply_committed(
        2,
        vec![
            TabletMutation::Delete { key: b"a".to_vec() },
            put(b"b", b"b2"),
        ],
    );

    assert_eq!(
        storage.scan_at(b"a", None, 2, 1).unwrap(),
        vec![(b"b".to_vec(), b"b2".to_vec())]
    );

    let _ = fs::remove_dir_all(&dir);
}
