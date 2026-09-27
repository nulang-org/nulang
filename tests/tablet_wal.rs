use std::fs;
use std::io::{Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use nulang::database::tablet::{
    KeyRange, TabletDescriptor, TabletId, TabletMutation, TabletWrite,
};
use nulang::database::wal::{FileWal, WalError};

static NEXT_TEST_WAL: AtomicU64 = AtomicU64::new(1);

fn temp_wal(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "nulang_nudb_{name}_{}_{}.wal",
        std::process::id(),
        NEXT_TEST_WAL.fetch_add(1, Ordering::Relaxed)
    ))
}

fn descriptor() -> TabletDescriptor {
    TabletDescriptor::new(
        TabletId::new(31).unwrap(),
        KeyRange::new(b"a".to_vec(), Some(b"z".to_vec())).unwrap(),
        4,
    )
    .unwrap()
}

fn write(previous: u64, key: &[u8], value: &[u8]) -> TabletWrite {
    TabletWrite::prepare(
        &descriptor(),
        4,
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
fn wal_reopens_and_recovers_committed_records_in_order() {
    let path = temp_wal("reopen");
    let _ = fs::remove_file(&path);

    {
        let mut wal = FileWal::open(&path).unwrap();
        wal.append_write(&write(0, b"k1", b"v1")).unwrap();
        wal.append_write(&write(1, b"k2", b"v2")).unwrap();
        assert_eq!(wal.last_sequence(), 2);
    }

    let wal = FileWal::open(&path).unwrap();
    assert_eq!(wal.records().len(), 2);
    assert_eq!(wal.records()[0].sequence(), 1);
    assert_eq!(wal.records()[1].sequence(), 2);
    assert_eq!(wal.records()[1].mutations()[0].key(), b"k2");

    let _ = fs::remove_file(path);
}

#[test]
fn wal_rejects_sequence_gaps_before_writing() {
    let path = temp_wal("sequence");
    let _ = fs::remove_file(&path);

    let mut wal = FileWal::open(&path).unwrap();
    wal.append_write(&write(0, b"k1", b"v1")).unwrap();

    let error = wal
        .append_write(
            &TabletWrite::prepare(
                &descriptor(),
                4,
                2,
                2,
                vec![TabletMutation::Put {
                    key: b"k3".to_vec(),
                    value: b"v3".to_vec(),
                }],
            )
            .unwrap(),
        )
        .unwrap_err();

    assert_eq!(
        error,
        WalError::SequenceMismatch {
            committed: 1,
            expected_previous: 2,
        }
    );
    assert_eq!(wal.records().len(), 1);

    let _ = fs::remove_file(path);
}

#[test]
fn wal_discards_a_truncated_crash_tail_but_keeps_prior_records() {
    let path = temp_wal("truncated");
    let _ = fs::remove_file(&path);

    {
        let mut wal = FileWal::open(&path).unwrap();
        wal.append_write(&write(0, b"k1", b"v1")).unwrap();
        wal.append_write(&write(1, b"k2", b"v2")).unwrap();
    }

    let valid_first_len = {
        let wal = FileWal::open(&path).unwrap();
        wal.record_end_offset(0).unwrap()
    };

    let file = fs::OpenOptions::new().write(true).open(&path).unwrap();
    file.set_len(valid_first_len + 7).unwrap();
    drop(file);

    let wal = FileWal::open(&path).unwrap();
    assert_eq!(wal.records().len(), 1);
    assert_eq!(wal.last_sequence(), 1);
    assert_eq!(fs::metadata(&path).unwrap().len(), valid_first_len);

    let _ = fs::remove_file(path);
}

#[test]
fn wal_fails_closed_on_checksum_corruption() {
    let path = temp_wal("checksum");
    let _ = fs::remove_file(&path);

    {
        let mut wal = FileWal::open(&path).unwrap();
        wal.append_write(&write(0, b"k1", b"value")).unwrap();
    }

    {
        let mut file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        file.seek(SeekFrom::Start(12)).unwrap();
        file.write_all(&[0xff]).unwrap();
        file.sync_all().unwrap();
    }

    let error = FileWal::open(&path).unwrap_err();
    assert!(matches!(error, WalError::ChecksumMismatch { .. }));

    let _ = fs::remove_file(path);
}

#[test]
fn wal_replay_reconstructs_mvcc_snapshots_after_restart() {
    let path = temp_wal("replay");
    let _ = fs::remove_file(&path);

    {
        let mut wal = FileWal::open(&path).unwrap();
        wal.append_write(&write(0, b"k", b"v1")).unwrap();
        wal.append_write(&write(1, b"k", b"v2")).unwrap();
    }

    let wal = FileWal::open(&path).unwrap();
    let tablet = wal.recover_memory_tablet(descriptor()).unwrap();

    assert_eq!(tablet.current_sequence(), 2);
    assert_eq!(tablet.read_at(b"k", 1).unwrap(), Some(&b"v1"[..]));
    assert_eq!(tablet.read_at(b"k", 2).unwrap(), Some(&b"v2"[..]));

    let _ = fs::remove_file(path);
}

#[test]
fn wal_replay_revalidates_mutations_against_the_recovery_range() {
    let path = temp_wal("replay_range");
    let _ = fs::remove_file(&path);

    {
        let mut wal = FileWal::open(&path).unwrap();
        wal.append_write(&write(0, b"y", b"value")).unwrap();
    }

    let narrow = TabletDescriptor::new(
        TabletId::new(31).unwrap(),
        KeyRange::new(b"a".to_vec(), Some(b"m".to_vec())).unwrap(),
        4,
    )
    .unwrap();

    let wal = FileWal::open(&path).unwrap();
    let error = wal.recover_memory_tablet(narrow).unwrap_err();
    assert!(matches!(
        error,
        WalError::ReplayRejected { sequence: 1, .. }
    ));

    let _ = fs::remove_file(path);
}


#[test]
fn wal_rejects_ownership_epoch_regression_on_append() {
    let path = temp_wal("epoch_regression");
    let _ = fs::remove_file(&path);

    let mut wal = FileWal::open(&path).unwrap();
    wal.append_write(&write(0, b"k1", b"v1")).unwrap();

    let stale_descriptor = TabletDescriptor::new(
        TabletId::new(31).unwrap(),
        KeyRange::new(b"a".to_vec(), Some(b"z".to_vec())).unwrap(),
        3,
    )
    .unwrap();
    let stale_write = TabletWrite::prepare(
        &stale_descriptor,
        3,
        1,
        1,
        vec![TabletMutation::Put {
            key: b"k2".to_vec(),
            value: b"stale".to_vec(),
        }],
    )
    .unwrap();

    assert_eq!(
        wal.append_write(&stale_write).unwrap_err(),
        WalError::StaleOwnershipEpoch {
            durable: 4,
            presented: 3,
        }
    );
    assert_eq!(wal.last_sequence(), 1);

    let _ = fs::remove_file(path);
}

#[test]
fn wal_recovery_rejects_a_descriptor_older_than_durable_ownership() {
    let path = temp_wal("stale_recovery_epoch");
    let _ = fs::remove_file(&path);

    {
        let mut wal = FileWal::open(&path).unwrap();
        wal.append_write(&write(0, b"k", b"value")).unwrap();
    }

    let stale_descriptor = TabletDescriptor::new(
        TabletId::new(31).unwrap(),
        KeyRange::new(b"a".to_vec(), Some(b"z".to_vec())).unwrap(),
        3,
    )
    .unwrap();

    let wal = FileWal::open(&path).unwrap();
    assert_eq!(
        wal.recover_memory_tablet(stale_descriptor).unwrap_err(),
        WalError::StaleOwnershipEpoch {
            durable: 4,
            presented: 3,
        }
    );

    let _ = fs::remove_file(path);
}
