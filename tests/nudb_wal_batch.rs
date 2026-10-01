use nulang::database::tablet::{KeyRange, MemoryTablet, TabletDescriptor, TabletId, TabletMutation, TabletWrite};
use nulang::database::wal_batch::BinaryBatchWal;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_WAL: AtomicU64 = AtomicU64::new(1);

fn temp_wal(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "nulang_nudb_batch_{name}_{}_{}.wal",
        std::process::id(),
        NEXT_WAL.fetch_add(1, Ordering::Relaxed)
    ))
}

fn descriptor() -> TabletDescriptor {
    TabletDescriptor::new(
        TabletId::new(1).unwrap(),
        KeyRange::new(b"a".to_vec(), Some(b"z".to_vec())).unwrap(),
        1,
    )
    .unwrap()
}

#[test]
fn append_batch_persists_multiple_consecutive_writes_with_one_batch_boundary() {
    let path = temp_wal("persist");
    let _ = std::fs::remove_file(&path);

    let mut tablet = MemoryTablet::new(descriptor());
    let write1 = tablet
        .prepare_write(
            1,
            0,
            vec![TabletMutation::Put {
                key: b"a".to_vec(),
                value: b"1".to_vec(),
            }],
        )
        .unwrap();
    tablet.commit(write1.clone()).unwrap();
    let write2 = tablet
        .prepare_write(
            1,
            1,
            vec![TabletMutation::Put {
                key: b"b".to_vec(),
                value: b"2".to_vec(),
            }],
        )
        .unwrap();

    let mut wal = BinaryBatchWal::open(&path).unwrap();
    wal.append_batch(&[write1, write2]).unwrap();
    drop(wal);

    let reopened = BinaryBatchWal::open(&path).unwrap();
    assert_eq!(reopened.last_sequence(), 2);
    assert_eq!(reopened.records().len(), 2);
    assert_eq!(reopened.records()[0].mutations()[0], TabletMutation::Put {
        key: b"a".to_vec(),
        value: b"1".to_vec(),
    });

    let _ = std::fs::remove_file(&path);
}

#[test]
fn append_batch_rejects_a_sequence_gap_before_emitting_any_record() {
    let path = temp_wal("gap");
    let _ = std::fs::remove_file(&path);

    let tablet = MemoryTablet::new(descriptor());
    let write1 = tablet
        .prepare_write(
            1,
            0,
            vec![TabletMutation::Put {
                key: b"a".to_vec(),
                value: b"1".to_vec(),
            }],
        )
        .unwrap();

    let write3 = TabletWrite::prepare(
        &descriptor(),
        1,
        2,
        2,
        vec![TabletMutation::Put {
            key: b"c".to_vec(),
            value: b"3".to_vec(),
        }],
    )
    .unwrap();

    let mut wal = BinaryBatchWal::open(&path).unwrap();
    assert!(wal.append_batch(&[write1, write3]).is_err());
    assert_eq!(wal.last_sequence(), 0);
    drop(wal);

    let reopened = BinaryBatchWal::open(&path).unwrap();
    assert_eq!(reopened.last_sequence(), 0);
    assert!(reopened.records().is_empty());

    let _ = std::fs::remove_file(&path);
}

#[test]
fn empty_batch_is_a_noop() {
    let path = temp_wal("empty");
    let _ = std::fs::remove_file(&path);

    let mut wal = BinaryBatchWal::open(&path).unwrap();
    wal.append_batch(&[]).unwrap();
    assert_eq!(wal.last_sequence(), 0);

    let _ = std::fs::remove_file(&path);
}
