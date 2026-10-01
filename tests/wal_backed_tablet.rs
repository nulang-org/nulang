use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use nulang::database::store::{WalBackedError, WalBackedTablet};
use nulang::database::tablet::{
    KeyRange, TabletDescriptor, TabletError, TabletId, TabletMutation, TabletWrite,
};
use nulang::database::wal::FileWal;

static NEXT_STORE: AtomicU64 = AtomicU64::new(1);

fn temp_wal(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "nulang_nudb_store_{name}_{}_{}.wal",
        std::process::id(),
        NEXT_STORE.fetch_add(1, Ordering::Relaxed)
    ))
}

fn descriptor() -> TabletDescriptor {
    TabletDescriptor::new(
        TabletId::new(41).unwrap(),
        KeyRange::new(b"a".to_vec(), Some(b"m".to_vec())).unwrap(),
        1,
    )
    .unwrap()
}

#[test]
fn wal_backed_commit_survives_restart_and_rebuilds_mvcc() {
    let path = temp_wal("restart");
    let _ = fs::remove_file(&path);

    {
        let mut tablet = WalBackedTablet::open(descriptor(), &path).unwrap();
        let write = tablet
            .prepare_write(
                1,
                0,
                vec![TabletMutation::Put {
                    key: b"k".to_vec(),
                    value: b"value".to_vec(),
                }],
            )
            .unwrap();
        tablet.commit(write).unwrap();

        assert_eq!(tablet.current_sequence(), 1);
        assert_eq!(tablet.read_latest(b"k"), Some(&b"value"[..]));
    }

    let tablet = WalBackedTablet::open(descriptor(), &path).unwrap();
    assert_eq!(tablet.current_sequence(), 1);
    assert_eq!(tablet.read_at(b"k", 1).unwrap(), Some(&b"value"[..]));

    let _ = fs::remove_file(path);
}

#[test]
fn invalid_write_is_rejected_before_it_reaches_the_wal() {
    let path = temp_wal("prevalidate");
    let _ = fs::remove_file(&path);

    let wider = TabletDescriptor::new(
        TabletId::new(41).unwrap(),
        KeyRange::new(b"a".to_vec(), Some(b"z".to_vec())).unwrap(),
        1,
    )
    .unwrap();
    let write = TabletWrite::prepare(
        &wider,
        1,
        0,
        0,
        vec![TabletMutation::Put {
            key: b"y".to_vec(),
            value: b"outside".to_vec(),
        }],
    )
    .unwrap();

    {
        let mut tablet = WalBackedTablet::open(descriptor(), &path).unwrap();
        assert_eq!(
            tablet.commit(write).unwrap_err(),
            WalBackedError::Tablet(TabletError::KeyOutsideTabletRange)
        );
        assert_eq!(tablet.current_sequence(), 0);
    }

    let wal = FileWal::open(&path).unwrap();
    assert_eq!(wal.last_sequence(), 0);

    let _ = fs::remove_file(path);
}

#[test]
fn stale_prepared_write_does_not_advance_the_wal_tail() {
    let path = temp_wal("stale");
    let _ = fs::remove_file(&path);

    {
        let mut tablet = WalBackedTablet::open(descriptor(), &path).unwrap();
        let first = tablet
            .prepare_write(
                1,
                0,
                vec![TabletMutation::Put {
                    key: b"k1".to_vec(),
                    value: b"first".to_vec(),
                }],
            )
            .unwrap();
        let stale = tablet
            .prepare_write(
                1,
                0,
                vec![TabletMutation::Put {
                    key: b"k2".to_vec(),
                    value: b"stale".to_vec(),
                }],
            )
            .unwrap();

        tablet.commit(first).unwrap();
        assert_eq!(
            tablet.commit(stale).unwrap_err(),
            WalBackedError::Tablet(TabletError::SequenceMismatch {
                committed: 1,
                expected_previous: 0,
            })
        );
    }

    let wal = FileWal::open(&path).unwrap();
    assert_eq!(wal.last_sequence(), 1);
    assert_eq!(wal.records().len(), 1);

    let _ = fs::remove_file(path);
}
