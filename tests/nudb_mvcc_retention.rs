//! Regression coverage for duplicate-key commits and persisted MVCC retention.
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use nulang::database::store::WalBackedTablet;
use nulang::database::tablet::{KeyRange, TabletDescriptor, TabletError, TabletId, TabletMutation};

static NEXT_TEST: AtomicU64 = AtomicU64::new(1);

fn temp_wal(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "nulang_nudb_mvcc_{name}_{}_{}.wal",
        std::process::id(),
        NEXT_TEST.fetch_add(1, Ordering::Relaxed)
    ))
}

fn descriptor() -> TabletDescriptor {
    TabletDescriptor::new(
        TabletId::new(81).unwrap(),
        KeyRange::new(b"a".to_vec(), Some(b"z".to_vec())).unwrap(),
        2,
    )
    .unwrap()
}

fn put(key: &[u8], value: &[u8]) -> TabletMutation {
    TabletMutation::Put {
        key: key.to_vec(),
        value: value.to_vec(),
    }
}

fn delete(key: &[u8]) -> TabletMutation {
    TabletMutation::Delete { key: key.to_vec() }
}

fn commit(tablet: &mut WalBackedTablet, mutations: Vec<TabletMutation>) {
    let previous = tablet.current_sequence();
    let write = tablet.prepare_write(2, previous, mutations).unwrap();
    tablet.commit(write).unwrap();
}

fn cleanup(path: &PathBuf) {
    let _ = fs::remove_file(path);
    let _ = fs::remove_file(path.with_extension("checkpoint"));
}

#[test]
fn duplicate_key_mutations_use_last_write_and_survive_checkpoint_reopen() {
    let path = temp_wal("duplicate_keys");
    cleanup(&path);

    {
        let mut tablet = WalBackedTablet::open(descriptor(), &path).unwrap();
        commit(
            &mut tablet,
            vec![
                put(b"k", b"old"),
                put(b"other", b"value"),
                put(b"k", b"winner"),
            ],
        );
        assert_eq!(tablet.read_at(b"k", 1).unwrap(), Some(&b"winner"[..]));
    }

    // Recover directly from the WAL before a checkpoint has been published.
    {
        let mut tablet = WalBackedTablet::open(descriptor(), &path).unwrap();
        assert_eq!(tablet.read_at(b"k", 1).unwrap(), Some(&b"winner"[..]));
        tablet.checkpoint().unwrap();
    }

    {
        let mut tablet = WalBackedTablet::open(descriptor(), &path).unwrap();
        assert_eq!(tablet.read_at(b"k", 1).unwrap(), Some(&b"winner"[..]));
        assert_eq!(tablet.read_latest(b"other"), Some(&b"value"[..]));
        commit(&mut tablet, vec![put(b"k", b"unused"), delete(b"k")]);
        assert_eq!(tablet.read_at(b"k", 2).unwrap(), None);
        tablet.checkpoint().unwrap();
    }

    let reopened = WalBackedTablet::open(descriptor(), &path).unwrap();
    assert_eq!(reopened.read_at(b"k", 1).unwrap(), Some(&b"winner"[..]));
    assert_eq!(reopened.read_at(b"k", 2).unwrap(), None);
    cleanup(&path);
}

#[test]
fn retention_keeps_snapshot_floor_and_rejects_expired_snapshots_after_restart() {
    let path = temp_wal("retention");
    cleanup(&path);

    {
        let mut tablet = WalBackedTablet::open(descriptor(), &path).unwrap();
        commit(&mut tablet, vec![put(b"k", b"v1"), put(b"deleted", b"old")]);
        commit(&mut tablet, vec![put(b"k", b"v2"), delete(b"deleted")]);
        commit(&mut tablet, vec![put(b"k", b"v3")]);

        tablet.reclaim_history_before(2).unwrap();
        assert_eq!(tablet.oldest_retained_snapshot(), 2);
        assert_eq!(tablet.read_at(b"k", 2).unwrap(), Some(&b"v2"[..]));
        assert_eq!(tablet.read_at(b"k", 3).unwrap(), Some(&b"v3"[..]));
        assert_eq!(tablet.read_at(b"deleted", 2).unwrap(), None);
        assert!(matches!(
            tablet.read_at(b"k", 1),
            Err(TabletError::SnapshotExpired {
                requested: 1,
                oldest_retained: 2
            })
        ));
    }

    {
        let mut tablet = WalBackedTablet::open(descriptor(), &path).unwrap();
        assert_eq!(tablet.oldest_retained_snapshot(), 2);
        assert_eq!(tablet.read_at(b"k", 2).unwrap(), Some(&b"v2"[..]));
        assert!(matches!(
            tablet.read_at(b"k", 1),
            Err(TabletError::SnapshotExpired {
                requested: 1,
                oldest_retained: 2
            })
        ));
        commit(&mut tablet, vec![put(b"k", b"v4")]);
        tablet.checkpoint().unwrap();
    }

    let reopened = WalBackedTablet::open(descriptor(), &path).unwrap();
    assert_eq!(reopened.oldest_retained_snapshot(), 2);
    assert_eq!(reopened.read_at(b"k", 4).unwrap(), Some(&b"v4"[..]));
    assert!(matches!(
        reopened.read_at(b"k", 0),
        Err(TabletError::SnapshotExpired {
            requested: 0,
            oldest_retained: 2
        })
    ));
    cleanup(&path);
}

#[test]
fn retention_rejects_future_watermark_without_changing_state() {
    let path = temp_wal("future_floor");
    cleanup(&path);

    let mut tablet = WalBackedTablet::open(descriptor(), &path).unwrap();
    commit(&mut tablet, vec![put(b"k", b"v1")]);
    assert!(tablet.reclaim_history_before(2).is_err());
    assert_eq!(tablet.oldest_retained_snapshot(), 0);
    assert_eq!(tablet.read_at(b"k", 0).unwrap(), None);
    assert_eq!(tablet.read_at(b"k", 1).unwrap(), Some(&b"v1"[..]));

    cleanup(&path);
}

#[test]
fn retention_floor_cannot_move_backward() {
    let path = temp_wal("monotonic");
    cleanup(&path);

    let mut tablet = WalBackedTablet::open(descriptor(), &path).unwrap();
    commit(&mut tablet, vec![put(b"k", b"v1")]);
    commit(&mut tablet, vec![put(b"k", b"v2")]);
    tablet.reclaim_history_before(2).unwrap();
    assert!(tablet.reclaim_history_before(1).is_err());
    assert_eq!(tablet.oldest_retained_snapshot(), 2);
    cleanup(&path);
}
