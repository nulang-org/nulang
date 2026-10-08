use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use nulang::database::store::{WalBackedError, WalBackedTablet};
use nulang::database::tablet::{
    KeyRange, TabletDescriptor, TabletError, TabletId, TabletMutation, TabletWrite,
};
use nulang::database::wal::FileWal;

static NEXT_TEST: AtomicU64 = AtomicU64::new(1);

fn temp_wal(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "nulang_nudb_group_commit_{name}_{}_{}.wal",
        std::process::id(),
        NEXT_TEST.fetch_add(1, Ordering::Relaxed)
    ))
}

fn descriptor() -> TabletDescriptor {
    TabletDescriptor::new(
        TabletId::new(61).unwrap(),
        KeyRange::new(Vec::new(), None).unwrap(),
        3,
    )
    .unwrap()
}

fn write(previous: u64, key: &[u8], value: &[u8]) -> TabletWrite {
    TabletWrite::prepare(
        &descriptor(),
        3,
        previous,
        previous,
        vec![TabletMutation::Put {
            key: key.to_vec(),
            value: value.to_vec(),
        }],
    )
    .unwrap()
}

#[test]
fn group_commit_publishes_consecutive_writes_after_one_durable_batch() {
    let path = temp_wal("restart");
    let _ = fs::remove_file(&path);

    {
        let mut tablet = WalBackedTablet::open(descriptor(), &path).unwrap();
        let committed = tablet
            .commit_batch(vec![write(0, b"a", b"one"), write(1, b"b", b"two")])
            .unwrap();
        assert_eq!(committed, 2);
        assert_eq!(tablet.current_sequence(), 2);
        assert_eq!(tablet.read_latest(b"a"), Some(&b"one"[..]));
        assert_eq!(tablet.read_latest(b"b"), Some(&b"two"[..]));
    }

    let reopened = WalBackedTablet::open(descriptor(), &path).unwrap();
    assert_eq!(reopened.current_sequence(), 2);
    assert_eq!(reopened.read_at(b"a", 1).unwrap(), Some(&b"one"[..]));
    assert_eq!(reopened.read_at(b"b", 2).unwrap(), Some(&b"two"[..]));

    let wal = FileWal::open(&path).unwrap();
    assert_eq!(wal.records().len(), 2);
    assert_eq!(wal.last_sequence(), 2);

    let _ = fs::remove_file(path);
}

#[test]
fn group_commit_prevalidates_the_entire_sequence_before_wal_mutation() {
    let path = temp_wal("prevalidate");
    let _ = fs::remove_file(&path);

    let first = write(0, b"a", b"one");
    let gap = TabletWrite::prepare(
        &descriptor(),
        3,
        2,
        2,
        vec![TabletMutation::Put {
            key: b"c".to_vec(),
            value: b"three".to_vec(),
        }],
    )
    .unwrap();

    {
        let mut tablet = WalBackedTablet::open(descriptor(), &path).unwrap();
        assert_eq!(
            tablet.commit_batch(vec![first, gap]).unwrap_err(),
            WalBackedError::Tablet(TabletError::SequenceMismatch {
                committed: 1,
                expected_previous: 2,
            })
        );
        assert_eq!(tablet.current_sequence(), 0);
        assert_eq!(tablet.read_latest(b"a"), None);
    }

    let wal = FileWal::open(&path).unwrap();
    assert_eq!(wal.last_sequence(), 0);
    assert!(wal.records().is_empty());

    let _ = fs::remove_file(path);
}

#[test]
fn empty_group_commit_is_a_noop() {
    let path = temp_wal("empty");
    let _ = fs::remove_file(&path);

    let mut tablet = WalBackedTablet::open(descriptor(), &path).unwrap();
    assert_eq!(tablet.commit_batch(Vec::new()).unwrap(), 0);
    assert_eq!(tablet.current_sequence(), 0);

    let _ = fs::remove_file(path);
}
