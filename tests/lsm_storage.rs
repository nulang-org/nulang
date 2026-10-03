use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use nulang::database::lsm::{LsmError, LsmStorage};
use nulang::database::tablet::{MvccStorage, TabletMutation};

static NEXT_TEST: AtomicU64 = AtomicU64::new(1);

fn temp_dir(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "nulang_nudb_lsm_{name}_{}_{}",
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

#[test]
fn flush_moves_memtable_history_into_an_immutable_table_and_reopens() {
    let dir = temp_dir("flush_reopen");
    let _ = fs::remove_dir_all(&dir);

    let table_path = {
        let mut storage = LsmStorage::open(&dir).unwrap();
        storage.apply_committed(1, vec![put(b"k", b"v1")]);
        storage.apply_committed(2, vec![put(b"k", b"v2"), put(b"other", b"x")]);

        assert_eq!(storage.current_sequence(), 2);
        assert_eq!(storage.table_count(), 0);
        assert_eq!(storage.mutable_version_count(), 3);
        assert_eq!(storage.read_at(b"k", 1), Some(&b"v1"[..]));
        assert_eq!(storage.read_at(b"k", 2), Some(&b"v2"[..]));

        let flushed = storage.flush().unwrap().expect("non-empty memtable");
        assert_eq!(flushed.min_sequence, 1);
        assert_eq!(flushed.max_sequence, 2);
        assert_eq!(flushed.version_count, 3);
        assert_eq!(storage.table_count(), 1);
        assert_eq!(storage.mutable_version_count(), 0);
        assert_eq!(storage.read_at(b"k", 1), Some(&b"v1"[..]));
        assert_eq!(storage.read_at(b"k", 2), Some(&b"v2"[..]));
        flushed.path
    };

    assert!(table_path.exists());

    let reopened = LsmStorage::open(&dir).unwrap();
    assert_eq!(reopened.current_sequence(), 2);
    assert_eq!(reopened.table_count(), 1);
    assert_eq!(reopened.mutable_version_count(), 0);
    assert_eq!(reopened.read_at(b"k", 1), Some(&b"v1"[..]));
    assert_eq!(reopened.read_at(b"k", 2), Some(&b"v2"[..]));
    assert_eq!(reopened.read_at(b"other", 2), Some(&b"x"[..]));

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn reads_choose_the_newest_visible_version_across_tables_and_memtable() {
    let dir = temp_dir("merge_reads");
    let _ = fs::remove_dir_all(&dir);

    let mut storage = LsmStorage::open(&dir).unwrap();
    storage.apply_committed(1, vec![put(b"k", b"v1")]);
    storage.flush().unwrap().unwrap();

    storage.apply_committed(2, vec![TabletMutation::Delete { key: b"k".to_vec() }]);
    storage.flush().unwrap().unwrap();

    storage.apply_committed(3, vec![put(b"k", b"v3")]);

    assert_eq!(storage.read_at(b"k", 1), Some(&b"v1"[..]));
    assert_eq!(storage.read_at(b"k", 2), None);
    assert_eq!(storage.read_at(b"k", 3), Some(&b"v3"[..]));

    drop(storage);
    let reopened = LsmStorage::open(&dir).unwrap();
    assert_eq!(reopened.current_sequence(), 2);
    assert_eq!(reopened.read_at(b"k", 1), Some(&b"v1"[..]));
    assert_eq!(reopened.read_at(b"k", 2), None);

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn flush_persists_sequence_advancement_from_empty_commits() {
    let dir = temp_dir("empty_commit");
    let _ = fs::remove_dir_all(&dir);

    {
        let mut storage = LsmStorage::open(&dir).unwrap();
        storage.apply_committed(1, vec![put(b"k", b"v1")]);
        storage.flush().unwrap().unwrap();

        storage.apply_committed(2, Vec::new());
        let flushed = storage.flush().unwrap().expect("sequence advanced");
        assert_eq!(flushed.min_sequence, 2);
        assert_eq!(flushed.max_sequence, 2);
        assert_eq!(flushed.version_count, 0);
    }

    let reopened = LsmStorage::open(&dir).unwrap();
    assert_eq!(reopened.current_sequence(), 2);
    assert_eq!(reopened.read_at(b"k", 2), Some(&b"v1"[..]));

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn empty_flush_is_a_noop() {
    let dir = temp_dir("empty_flush");
    let _ = fs::remove_dir_all(&dir);

    let mut storage = LsmStorage::open(&dir).unwrap();
    assert!(storage.flush().unwrap().is_none());
    assert_eq!(storage.table_count(), 0);

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn corrupt_immutable_table_fails_closed_on_reopen() {
    let dir = temp_dir("corrupt");
    let _ = fs::remove_dir_all(&dir);

    let path = {
        let mut storage = LsmStorage::open(&dir).unwrap();
        storage.apply_committed(1, vec![put(b"k", b"value")]);
        storage.flush().unwrap().unwrap().path
    };

    let mut bytes = fs::read(&path).unwrap();
    let index = bytes.len() / 2;
    bytes[index] ^= 0xff;
    fs::write(&path, bytes).unwrap();

    assert!(matches!(
        LsmStorage::open(&dir),
        Err(LsmError::ChecksumMismatch { .. })
            | Err(LsmError::CorruptTable { .. })
            | Err(LsmError::UnsupportedVersion { .. })
    ));

    let _ = fs::remove_dir_all(&dir);
}
