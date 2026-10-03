use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use nulang::database::store::WalBackedTablet;
use nulang::database::tablet::{KeyRange, TabletDescriptor, TabletId, TabletMutation};
use nulang::database::wal::FileWal;

static NEXT_TEST: AtomicU64 = AtomicU64::new(1);

fn temp_wal(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "nulang_nudb_l0_compaction_{name}_{}_{}.wal",
        std::process::id(),
        NEXT_TEST.fetch_add(1, Ordering::Relaxed)
    ))
}

fn descriptor() -> TabletDescriptor {
    TabletDescriptor::new(
        TabletId::new(905).unwrap(),
        KeyRange::new(b"a".to_vec(), Some(b"z".to_vec())).unwrap(),
        1,
    )
    .unwrap()
}

fn cleanup(path: &PathBuf) {
    let manifest = path.with_extension("manifest");
    let mut manifest_tmp = manifest.as_os_str().to_os_string();
    manifest_tmp.push(".tmp");
    let _ = fs::remove_file(path);
    let _ = fs::remove_file(path.with_extension("checkpoint"));
    let _ = fs::remove_file(&manifest);
    let _ = fs::remove_file(PathBuf::from(manifest_tmp));
    let _ = fs::remove_dir_all(path.with_extension("sstables"));
}

fn commit_put(tablet: &mut WalBackedTablet, key: &[u8], value: &[u8]) {
    let sequence = tablet.current_sequence();
    let write = tablet
        .prepare_write(
            1,
            sequence,
            vec![TabletMutation::Put {
                key: key.to_vec(),
                value: value.to_vec(),
            }],
        )
        .unwrap();
    tablet.commit(write).unwrap();
}

fn commit_delete(tablet: &mut WalBackedTablet, key: &[u8]) {
    let sequence = tablet.current_sequence();
    let write = tablet
        .prepare_write(
            1,
            sequence,
            vec![TabletMutation::Delete { key: key.to_vec() }],
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

fn sstable_file_count(path: &PathBuf) -> usize {
    let dir = path.with_extension("sstables");
    fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "sst"))
                .count()
        })
        .unwrap_or(0)
}

#[test]
fn l0_compaction_is_a_noop_below_the_four_table_trigger() {
    let path = temp_wal("below_trigger");
    cleanup(&path);

    let mut tablet = WalBackedTablet::open(descriptor(), &path).unwrap();
    for value in [b"v1".as_slice(), b"v2".as_slice(), b"v3".as_slice()] {
        commit_put(&mut tablet, b"k", value);
        flush_current(&mut tablet);
    }

    assert_eq!(tablet.durable_sstable_count().unwrap(), 3);
    assert_eq!(sstable_file_count(&path), 3);
    assert!(!tablet.compact_l0_once().unwrap());
    assert_eq!(tablet.durable_sstable_count().unwrap(), 3);
    assert_eq!(sstable_file_count(&path), 3);

    cleanup(&path);
}

#[test]
fn l0_compaction_merges_four_tables_preserves_mvcc_and_retires_sources() {
    let path = temp_wal("merge");
    cleanup(&path);

    let mut tablet = WalBackedTablet::open(descriptor(), &path).unwrap();
    for value in [
        b"v1".as_slice(),
        b"v2".as_slice(),
        b"v3".as_slice(),
        b"v4".as_slice(),
    ] {
        commit_put(&mut tablet, b"k", value);
        flush_current(&mut tablet);
    }

    assert_eq!(tablet.durable_sstable_count().unwrap(), 4);
    assert_eq!(sstable_file_count(&path), 4);
    assert!(tablet.compact_l0_once().unwrap());
    assert_eq!(tablet.durable_sstable_count().unwrap(), 1);
    assert_eq!(sstable_file_count(&path), 1);

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

    drop(tablet);
    let reopened = WalBackedTablet::open(descriptor(), &path).unwrap();
    assert_eq!(reopened.current_sequence(), 4);
    assert_eq!(reopened.durable_sstable_count().unwrap(), 1);
    assert_eq!(sstable_file_count(&path), 1);
    for (snapshot, expected) in [
        (1, b"v1".as_slice()),
        (2, b"v2".as_slice()),
        (3, b"v3".as_slice()),
        (4, b"v4".as_slice()),
    ] {
        assert_eq!(
            reopened.read_at(b"k", snapshot).unwrap().as_deref(),
            Some(expected)
        );
    }

    cleanup(&path);
}

#[test]
fn l0_compaction_preserves_tombstones_and_later_reinsertion() {
    let path = temp_wal("tombstone");
    cleanup(&path);

    let mut tablet = WalBackedTablet::open(descriptor(), &path).unwrap();
    commit_put(&mut tablet, b"k", b"v1");
    flush_current(&mut tablet);
    commit_put(&mut tablet, b"k", b"v2");
    flush_current(&mut tablet);
    commit_delete(&mut tablet, b"k");
    flush_current(&mut tablet);
    commit_put(&mut tablet, b"k", b"v4");
    flush_current(&mut tablet);

    assert!(tablet.compact_l0_once().unwrap());
    assert_eq!(
        tablet.read_at(b"k", 1).unwrap().as_deref(),
        Some(&b"v1"[..])
    );
    assert_eq!(
        tablet.read_at(b"k", 2).unwrap().as_deref(),
        Some(&b"v2"[..])
    );
    assert_eq!(tablet.read_at(b"k", 3).unwrap(), None);
    assert_eq!(
        tablet.read_at(b"k", 4).unwrap().as_deref(),
        Some(&b"v4"[..])
    );

    drop(tablet);
    let reopened = WalBackedTablet::open(descriptor(), &path).unwrap();
    assert_eq!(
        reopened.read_at(b"k", 1).unwrap().as_deref(),
        Some(&b"v1"[..])
    );
    assert_eq!(
        reopened.read_at(b"k", 2).unwrap().as_deref(),
        Some(&b"v2"[..])
    );
    assert_eq!(reopened.read_at(b"k", 3).unwrap(), None);
    assert_eq!(
        reopened.read_at(b"k", 4).unwrap().as_deref(),
        Some(&b"v4"[..])
    );
    cleanup(&path);
}

#[test]
fn compacted_table_recovers_after_the_covered_wal_prefix_is_reclaimed() {
    let path = temp_wal("reclaimed_wal");
    cleanup(&path);

    let mut tablet = WalBackedTablet::open(descriptor(), &path).unwrap();
    for (key, value) in [
        (b"a".as_slice(), b"one".as_slice()),
        (b"b".as_slice(), b"two".as_slice()),
        (b"c".as_slice(), b"three".as_slice()),
        (b"d".as_slice(), b"four".as_slice()),
    ] {
        commit_put(&mut tablet, key, value);
        flush_current(&mut tablet);
    }
    assert!(tablet.compact_l0_once().unwrap());
    drop(tablet);

    let mut wal = FileWal::open(&path).unwrap();
    wal.reclaim_through(4).unwrap();
    drop(wal);

    let reopened = WalBackedTablet::open(descriptor(), &path).unwrap();
    assert_eq!(reopened.current_sequence(), 4);
    assert_eq!(reopened.durable_sstable_count().unwrap(), 1);
    assert_eq!(
        reopened.read_latest(b"a").unwrap().as_deref(),
        Some(&b"one"[..])
    );
    assert_eq!(
        reopened.read_latest(b"b").unwrap().as_deref(),
        Some(&b"two"[..])
    );
    assert_eq!(
        reopened.read_latest(b"c").unwrap().as_deref(),
        Some(&b"three"[..])
    );
    assert_eq!(
        reopened.read_latest(b"d").unwrap().as_deref(),
        Some(&b"four"[..])
    );

    cleanup(&path);
}
