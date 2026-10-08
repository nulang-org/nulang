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

#[test]
fn second_writable_handle_is_rejected_without_changing_the_wal() {
    let path = temp_wal("exclusive_writer");
    let _ = fs::remove_file(&path);

    let mut writer = WalBackedTablet::open(descriptor(), &path).unwrap();
    let initial_size = fs::metadata(&path).unwrap().len();

    assert!(
        WalBackedTablet::open(descriptor(), &path).is_err(),
        "a second live writer must not be admitted with the same sequence"
    );
    assert_eq!(fs::metadata(&path).unwrap().len(), initial_size);

    let write = writer
        .prepare_write(
            1,
            0,
            vec![TabletMutation::Put {
                key: b"k".to_vec(),
                value: b"acknowledged".to_vec(),
            }],
        )
        .unwrap();
    assert_eq!(writer.commit(write).unwrap(), 1);
    assert!(
        WalBackedTablet::open(descriptor(), &path).is_err(),
        "a writer must remain exclusive after committing"
    );

    drop(writer);
    let reopened = WalBackedTablet::open(descriptor(), &path).unwrap();
    assert_eq!(reopened.current_sequence(), 1);
    assert_eq!(reopened.read_latest(b"k"), Some(&b"acknowledged"[..]));
    drop(reopened);

    let _ = fs::remove_file(&path);
}

#[test]
fn writer_ownership_survives_wal_reclamation_and_relative_path_aliases() {
    let path = temp_wal("exclusive_reclaim");
    let _ = fs::remove_file(&path);
    let _ = fs::remove_file(path.with_extension("checkpoint"));

    let mut writer = WalBackedTablet::open(descriptor(), &path).unwrap();
    let write = writer
        .prepare_write(
            1,
            0,
            vec![TabletMutation::Put {
                key: b"k".to_vec(),
                value: b"value".to_vec(),
            }],
        )
        .unwrap();
    writer.commit(write).unwrap();
    writer.checkpoint().unwrap();

    // Two lexical paths to the same physical WAL must share one writer lock.
    let alias = path.parent().unwrap().join(".").join(path.file_name().unwrap());
    assert!(
        WalBackedTablet::open(descriptor(), &alias).is_err(),
        "checkpoint rename and parent aliases must not bypass the writer lock"
    );
    assert_eq!(writer.current_sequence(), 1);

    drop(writer);
    let reopened = WalBackedTablet::open(descriptor(), &alias).unwrap();
    assert_eq!(reopened.read_latest(b"k"), Some(&b"value"[..]));
    drop(reopened);

    let _ = fs::remove_file(&path);
    let _ = fs::remove_file(path.with_extension("checkpoint"));
}
