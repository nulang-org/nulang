use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use nulang::database::store::WalBackedTablet;
use nulang::database::tablet::{KeyRange, TabletDescriptor, TabletId, TabletMutation};

static NEXT_TEST: AtomicU64 = AtomicU64::new(1);

fn temp_wal(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "nulang_nudb_sstable_serving_{name}_{}_{}.wal",
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

#[test]
fn flushed_sstable_remains_visible_after_resident_memtable_eviction() {
    let path = temp_wal("resident_eviction");
    cleanup(&path);
    let mut tablet = WalBackedTablet::open(descriptor(), &path).unwrap();

    commit_put(&mut tablet, b"k", b"v1");
    flush_current(&mut tablet);

    assert_eq!(tablet.immutable_memtable_count(), 0);
    assert_eq!(
        tablet.read_at(b"k", 1).unwrap().as_deref(),
        Some(&b"v1"[..])
    );

    commit_put(&mut tablet, b"k", b"v2");
    assert_eq!(
        tablet.read_at(b"k", 1).unwrap().as_deref(),
        Some(&b"v1"[..])
    );
    assert_eq!(
        tablet.read_at(b"k", 2).unwrap().as_deref(),
        Some(&b"v2"[..])
    );
    cleanup(&path);
}

#[test]
fn newer_sstable_tombstone_wins_without_hiding_older_snapshot() {
    let path = temp_wal("tombstone");
    cleanup(&path);
    let mut tablet = WalBackedTablet::open(descriptor(), &path).unwrap();

    commit_put(&mut tablet, b"k", b"v1");
    flush_current(&mut tablet);
    commit_delete(&mut tablet, b"k");
    flush_current(&mut tablet);

    assert_eq!(tablet.immutable_memtable_count(), 0);
    assert_eq!(
        tablet.read_at(b"k", 1).unwrap().as_deref(),
        Some(&b"v1"[..])
    );
    assert_eq!(tablet.read_at(b"k", 2).unwrap(), None);
    assert_eq!(tablet.read_latest(b"k").unwrap(), None);
    cleanup(&path);
}

#[test]
fn reopen_serves_manifest_sstable_without_rehydrating_memtable() {
    let path = temp_wal("reopen");
    cleanup(&path);
    {
        let mut tablet = WalBackedTablet::open(descriptor(), &path).unwrap();
        commit_put(&mut tablet, b"k", b"v1");
        flush_current(&mut tablet);
    }

    let reopened = WalBackedTablet::open(descriptor(), &path).unwrap();
    assert_eq!(reopened.current_sequence(), 1);
    assert_eq!(reopened.immutable_memtable_count(), 0);
    assert_eq!(
        reopened.read_latest(b"k").unwrap().as_deref(),
        Some(&b"v1"[..])
    );
    cleanup(&path);
}

#[test]
fn checkpoint_composes_sstable_and_memory_before_wal_reclamation() {
    let path = temp_wal("checkpoint");
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
        reopened.read_latest(b"a").unwrap().as_deref(),
        Some(&b"from-sstable"[..])
    );
    assert_eq!(
        reopened.read_latest(b"b").unwrap().as_deref(),
        Some(&b"from-memtable"[..])
    );
    cleanup(&path);
}
