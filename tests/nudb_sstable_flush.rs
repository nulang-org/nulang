use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use nulang::database::store::WalBackedTablet;
use nulang::database::tablet::{KeyRange, TabletDescriptor, TabletId, TabletMutation};
use nulang::database::wal::FileWal;

static NEXT_TEST: AtomicU64 = AtomicU64::new(1);

fn temp_wal() -> PathBuf {
    std::env::temp_dir().join(format!(
        "nulang_nudb_sstable_flush_{}_{}.wal",
        std::process::id(),
        NEXT_TEST.fetch_add(1, Ordering::Relaxed)
    ))
}

fn descriptor() -> TabletDescriptor {
    TabletDescriptor::new(
        TabletId::new(903).unwrap(),
        KeyRange::new(b"a".to_vec(), Some(b"z".to_vec())).unwrap(),
        1,
    )
    .unwrap()
}

fn cleanup(path: &PathBuf) {
    let _ = fs::remove_file(path);
    let _ = fs::remove_file(path.with_extension("checkpoint"));
    let _ = fs::remove_file(path.with_extension("manifest"));
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

fn rotate_current(tablet: &mut WalBackedTablet) {
    let bytes = tablet.mutable_memtable_bytes();
    assert!(bytes > 0);
    assert!(tablet.rotate_memtable_if_bytes_at_least(bytes));
}

#[test]
fn immutable_flush_is_durable_idempotent_evicts_memory_and_does_not_reclaim_wal() {
    let path = temp_wal();
    cleanup(&path);
    let mut tablet = WalBackedTablet::open(descriptor(), &path).unwrap();
    commit_put(&mut tablet, b"k", b"v1");
    flush_current(&mut tablet);

    assert_eq!(tablet.durable_sstable_count().unwrap(), 1);
    assert_eq!(
        tablet.immutable_memtable_count(),
        0,
        "a manifest-published SSTable should own the flushed generation"
    );
    assert_eq!(tablet.read_latest(b"k").unwrap(), Some(&b"v1"[..]));
    assert!(!tablet.flush_oldest_immutable_to_sstable().unwrap());
    assert_eq!(tablet.durable_sstable_count().unwrap(), 1);

    let wal = FileWal::open(&path).unwrap();
    assert_eq!(wal.base_sequence(), 0);
    assert_eq!(wal.records().len(), 1);
    drop(wal);
    drop(tablet);

    let reopened = WalBackedTablet::open(descriptor(), &path).unwrap();
    assert_eq!(reopened.current_sequence(), 1);
    assert_eq!(reopened.immutable_memtable_count(), 0);
    assert_eq!(reopened.read_latest(b"k").unwrap(), Some(&b"v1"[..]));
    assert_eq!(reopened.durable_sstable_count().unwrap(), 1);
    cleanup(&path);
}

#[test]
fn flush_evicts_only_the_oldest_generation_with_newer_memory_still_live() {
    let path = temp_wal();
    cleanup(&path);
    let mut tablet = WalBackedTablet::open(descriptor(), &path).unwrap();

    commit_put(&mut tablet, b"k", b"v1");
    rotate_current(&mut tablet);
    commit_put(&mut tablet, b"k", b"v2");
    rotate_current(&mut tablet);
    commit_put(&mut tablet, b"k", b"v3");
    assert_eq!(tablet.immutable_memtable_count(), 2);

    assert!(tablet.flush_oldest_immutable_to_sstable().unwrap());
    assert_eq!(tablet.immutable_memtable_count(), 1);
    assert_eq!(tablet.read_at(b"k", 1).unwrap(), Some(&b"v1"[..]));
    assert_eq!(tablet.read_at(b"k", 2).unwrap(), Some(&b"v2"[..]));
    assert_eq!(tablet.read_latest(b"k").unwrap(), Some(&b"v3"[..]));

    assert!(tablet.flush_oldest_immutable_to_sstable().unwrap());
    assert_eq!(tablet.immutable_memtable_count(), 0);
    assert_eq!(tablet.read_at(b"k", 1).unwrap(), Some(&b"v1"[..]));
    assert_eq!(tablet.read_at(b"k", 2).unwrap(), Some(&b"v2"[..]));
    assert_eq!(tablet.read_latest(b"k").unwrap(), Some(&b"v3"[..]));
    assert_eq!(tablet.durable_sstable_count().unwrap(), 2);

    cleanup(&path);
}

#[test]
fn newer_mutable_value_masks_flushed_sstable_value() {
    let path = temp_wal();
    cleanup(&path);
    let mut tablet = WalBackedTablet::open(descriptor(), &path).unwrap();
    commit_put(&mut tablet, b"k", b"v1");
    flush_current(&mut tablet);
    commit_put(&mut tablet, b"k", b"v2");

    assert_eq!(tablet.read_at(b"k", 1).unwrap(), Some(&b"v1"[..]));
    assert_eq!(tablet.read_latest(b"k").unwrap(), Some(&b"v2"[..]));
    cleanup(&path);
}

#[test]
fn newer_mutable_tombstone_masks_flushed_sstable_value() {
    let path = temp_wal();
    cleanup(&path);
    let mut tablet = WalBackedTablet::open(descriptor(), &path).unwrap();
    commit_put(&mut tablet, b"k", b"v1");
    flush_current(&mut tablet);
    commit_delete(&mut tablet, b"k");

    assert_eq!(tablet.read_at(b"k", 1).unwrap(), Some(&b"v1"[..]));
    assert_eq!(tablet.read_at(b"k", 2).unwrap(), None);
    assert_eq!(tablet.read_latest(b"k").unwrap(), None);
    cleanup(&path);
}

#[test]
fn checkpoint_is_self_contained_after_sstable_eviction_and_wal_reclamation() {
    let path = temp_wal();
    cleanup(&path);
    {
        let mut tablet = WalBackedTablet::open(descriptor(), &path).unwrap();
        commit_put(&mut tablet, b"a", b"from-sstable");
        flush_current(&mut tablet);
        commit_put(&mut tablet, b"b", b"from-memtable");

        tablet.checkpoint().unwrap();
    }

    fs::remove_file(path.with_extension("manifest")).unwrap();
    fs::remove_dir_all(path.with_extension("sstables")).unwrap();

    let reopened = WalBackedTablet::open(descriptor(), &path).unwrap();
    assert_eq!(reopened.current_sequence(), 2);
    assert_eq!(
        reopened.read_latest(b"a").unwrap(),
        Some(&b"from-sstable"[..])
    );
    assert_eq!(
        reopened.read_latest(b"b").unwrap(),
        Some(&b"from-memtable"[..])
    );
    cleanup(&path);
}
