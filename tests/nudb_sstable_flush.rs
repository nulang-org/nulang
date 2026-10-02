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

#[test]
fn immutable_flush_is_durable_idempotent_and_does_not_reclaim_wal() {
    let path = temp_wal();
    cleanup(&path);
    let mut tablet = WalBackedTablet::open(descriptor(), &path).unwrap();
    let write = tablet
        .prepare_write(
            1,
            0,
            vec![TabletMutation::Put {
                key: b"k".to_vec(),
                value: b"v1".to_vec(),
            }],
        )
        .unwrap();
    tablet.commit(write).unwrap();
    let bytes = tablet.mutable_memtable_bytes();
    assert!(tablet.rotate_memtable_if_bytes_at_least(bytes));

    assert!(tablet.flush_oldest_immutable_to_sstable().unwrap());
    assert_eq!(tablet.durable_sstable_count().unwrap(), 1);
    assert!(!tablet.flush_oldest_immutable_to_sstable().unwrap());
    assert_eq!(tablet.durable_sstable_count().unwrap(), 1);
    assert_eq!(
        tablet.immutable_memtable_count(),
        1,
        "flush does not evict until SSTable serving recovery exists"
    );

    let wal = FileWal::open(&path).unwrap();
    assert_eq!(wal.base_sequence(), 0);
    assert_eq!(wal.records().len(), 1);
    drop(wal);
    drop(tablet);

    let reopened = WalBackedTablet::open(descriptor(), &path).unwrap();
    assert_eq!(reopened.current_sequence(), 1);
    assert_eq!(reopened.read_latest(b"k"), Some(&b"v1"[..]));
    assert_eq!(reopened.durable_sstable_count().unwrap(), 1);
    cleanup(&path);
}
