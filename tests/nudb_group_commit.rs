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
    // An independent writable WAL inspector must not overlap the tablet owner.
    // This also permits a future exclusive single-writer lock on FileWal.
    drop(reopened);

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

#[test]
fn batch_append_preserves_the_exact_single_record_wal_format() {
    let serial_path = temp_wal("serial_format");
    let batch_path = temp_wal("batch_format");
    let _ = fs::remove_file(&serial_path);
    let _ = fs::remove_file(&batch_path);

    // Multiple frames and multiple mutation shapes must have identical
    // framing, JSON payload bytes, and checksums whether appended singly
    // or durably as one group. This protects borrowed WAL serialization.
    let first = TabletWrite::prepare(
        &descriptor(),
        3,
        0,
        0,
        vec![
            TabletMutation::Put {
                key: b"a".to_vec(),
                value: b"before".to_vec(),
            },
            TabletMutation::Delete { key: b"a".to_vec() },
            TabletMutation::Put {
                key: b"a".to_vec(),
                value: b"after".to_vec(),
            },
        ],
    )
    .unwrap();
    let second = write(1, b"b", b"second");
    let writes = [first, second];

    {
        let mut serial = FileWal::open(&serial_path).unwrap();
        for item in &writes {
            serial.append_write(item).unwrap();
        }
    }
    {
        let mut batched = FileWal::open(&batch_path).unwrap();
        batched.append_batch(&writes).unwrap();
    }

    assert_eq!(
        fs::read(&serial_path).unwrap(),
        fs::read(&batch_path).unwrap(),
        "one-sync batch encoding must match individually persisted WAL frames"
    );

    let recovered = FileWal::open(&batch_path).unwrap();
    assert_eq!(recovered.last_sequence(), 2);
    assert_eq!(recovered.records().len(), 2);
    drop(recovered);

    let _ = fs::remove_file(serial_path);
    let _ = fs::remove_file(batch_path);
}
