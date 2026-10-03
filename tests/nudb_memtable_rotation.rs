use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use nulang::database::store::WalBackedTablet;
use nulang::database::tablet::{KeyRange, TabletDescriptor, TabletId, TabletMutation};

static NEXT_TEST: AtomicU64 = AtomicU64::new(1);

fn temp_wal() -> PathBuf {
    std::env::temp_dir().join(format!(
        "nulang_nudb_memtable_{}_{}.wal",
        std::process::id(),
        NEXT_TEST.fetch_add(1, Ordering::Relaxed)
    ))
}

fn descriptor() -> TabletDescriptor {
    TabletDescriptor::new(
        TabletId::new(902).unwrap(),
        KeyRange::new(b"a".to_vec(), Some(b"z".to_vec())).unwrap(),
        1,
    )
    .unwrap()
}

fn put(value: &[u8]) -> Vec<TabletMutation> {
    vec![TabletMutation::Put {
        key: b"k".to_vec(),
        value: value.to_vec(),
    }]
}

fn cleanup(path: &PathBuf) {
    let _ = fs::remove_file(path);
    let _ = fs::remove_file(path.with_extension("checkpoint"));
}

#[test]
fn wal_backed_rotation_checkpoint_and_reopen_preserve_mvcc_history() {
    let path = temp_wal();
    cleanup(&path);
    let mut tablet = WalBackedTablet::open(descriptor(), &path).unwrap();

    let w1 = tablet.prepare_write(1, 0, put(b"v1")).unwrap();
    tablet.commit(w1).unwrap();
    let bytes = tablet.mutable_memtable_bytes();
    assert!(bytes > 0);
    assert!(tablet.rotate_memtable_if_bytes_at_least(bytes));
    assert_eq!(tablet.immutable_memtable_count(), 1);

    let w2 = tablet.prepare_write(1, 1, put(b"v2")).unwrap();
    tablet.commit(w2).unwrap();
    assert_eq!(tablet.read_at(b"k", 1).unwrap(), Some(&b"v1"[..]));
    assert_eq!(tablet.read_latest(b"k").unwrap(), Some(&b"v2"[..]));

    tablet.checkpoint().unwrap();
    drop(tablet);

    let reopened = WalBackedTablet::open(descriptor(), &path).unwrap();
    assert_eq!(reopened.current_sequence(), 2);
    assert_eq!(reopened.read_at(b"k", 1).unwrap(), Some(&b"v1"[..]));
    assert_eq!(reopened.read_latest(b"k").unwrap(), Some(&b"v2"[..]));
    // Checkpoint restoration becomes one immutable baseline and leaves replay/new
    // writes in the mutable generation.
    assert_eq!(reopened.immutable_memtable_count(), 1);
    assert_eq!(reopened.mutable_memtable_bytes(), 0);

    cleanup(&path);
}
