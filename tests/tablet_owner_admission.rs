use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use nulang::database::owner::{
    TabletAdmissionError, TabletOwner, TabletOwnerConfig, TabletOwnerState,
};
use nulang::database::tablet::{KeyRange, TabletDescriptor, TabletId, TabletMutation};

static NEXT_TEST: AtomicU64 = AtomicU64::new(1);

fn temp_wal(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "nulang_nudb_owner_{name}_{}_{}.wal",
        std::process::id(),
        NEXT_TEST.fetch_add(1, Ordering::Relaxed)
    ))
}

fn descriptor(epoch: u64) -> TabletDescriptor {
    TabletDescriptor::new(
        TabletId::new(121).unwrap(),
        KeyRange::new(b"a".to_vec(), Some(b"z".to_vec())).unwrap(),
        epoch,
    )
    .unwrap()
}

fn cleanup(path: &PathBuf) {
    let _ = fs::remove_file(path);
    let _ = fs::remove_file(path.with_extension("checkpoint"));
}

fn put(value: &[u8]) -> Vec<TabletMutation> {
    vec![TabletMutation::Put {
        key: b"k".to_vec(),
        value: value.to_vec(),
    }]
}

#[test]
fn owner_serializes_admitted_writes_in_fifo_order() {
    let path = temp_wal("fifo");
    cleanup(&path);

    let mut owner = TabletOwner::open(
        descriptor(11),
        &path,
        TabletOwnerConfig {
            queue_capacity: 4,
            ..Default::default()
        },
    )
    .unwrap();

    let first = owner.admit_write(11, 0, put(b"v1")).unwrap();
    let second = owner.admit_write(11, 1, put(b"v2")).unwrap();

    assert_eq!(owner.queued_writes(), 2);
    assert_eq!(owner.process_next().unwrap().request_id, first);
    assert_eq!(owner.process_next().unwrap().request_id, second);
    assert_eq!(owner.current_sequence().unwrap(), 2);
    assert_eq!(owner.read_at(b"k", 1).unwrap(), Some(&b"v1"[..]));
    assert_eq!(owner.read_at(b"k", 2).unwrap(), Some(&b"v2"[..]));
    assert_eq!(owner.read_latest(b"k").unwrap(), Some(&b"v2"[..]));

    cleanup(&path);
}

#[test]
fn owner_rejects_over_capacity_without_dropping_queued_work() {
    let path = temp_wal("backpressure");
    cleanup(&path);

    let mut owner = TabletOwner::open(
        descriptor(13),
        &path,
        TabletOwnerConfig {
            queue_capacity: 2,
            ..Default::default()
        },
    )
    .unwrap();

    let first = owner.admit_write(13, 0, put(b"v1")).unwrap();
    let second = owner.admit_write(13, 1, put(b"v2")).unwrap();

    assert_eq!(
        owner.admit_write(13, 2, put(b"v3")).unwrap_err(),
        TabletAdmissionError::Backpressured {
            capacity: 2,
            queued: 2,
        }
    );

    assert_eq!(owner.process_next().unwrap().request_id, first);
    assert_eq!(owner.process_next().unwrap().request_id, second);
    assert_eq!(owner.current_sequence().unwrap(), 2);

    cleanup(&path);
}

#[test]
fn owner_fences_stale_and_future_epochs_before_admission() {
    let path = temp_wal("epoch");
    cleanup(&path);

    let mut owner = TabletOwner::open(
        descriptor(17),
        &path,
        TabletOwnerConfig {
            queue_capacity: 4,
            ..Default::default()
        },
    )
    .unwrap();

    assert!(matches!(
        owner.admit_write(16, 0, put(b"stale")),
        Err(TabletAdmissionError::StaleEpoch {
            current: 17,
            presented: 16
        })
    ));
    assert!(matches!(
        owner.admit_write(18, 0, put(b"future")),
        Err(TabletAdmissionError::UnknownEpoch {
            current: 17,
            presented: 18
        })
    ));
    assert_eq!(owner.queued_writes(), 0);

    cleanup(&path);
}

#[test]
fn draining_owner_rejects_new_work_and_finishes_existing_queue() {
    let path = temp_wal("drain");
    cleanup(&path);

    let mut owner = TabletOwner::open(
        descriptor(23),
        &path,
        TabletOwnerConfig {
            queue_capacity: 4,
            ..Default::default()
        },
    )
    .unwrap();

    let first = owner.admit_write(23, 0, put(b"v1")).unwrap();
    let second = owner.admit_write(23, 1, put(b"v2")).unwrap();

    owner.begin_drain(24).unwrap();
    assert_eq!(owner.state(), TabletOwnerState::Draining { next_epoch: 24 });
    assert_eq!(
        owner.admit_write(23, 2, put(b"late")).unwrap_err(),
        TabletAdmissionError::Draining { next_epoch: 24 }
    );

    assert_eq!(owner.process_next().unwrap().request_id, first);
    assert_eq!(owner.process_next().unwrap().request_id, second);
    assert!(owner.is_drained());
    assert_eq!(owner.current_sequence().unwrap(), 2);

    cleanup(&path);
}

#[test]
fn drain_epoch_must_advance_ownership() {
    let path = temp_wal("drain_epoch");
    cleanup(&path);

    let mut owner = TabletOwner::open(
        descriptor(31),
        &path,
        TabletOwnerConfig {
            queue_capacity: 1,
            ..Default::default()
        },
    )
    .unwrap();

    assert_eq!(
        owner.begin_drain(31).unwrap_err(),
        TabletAdmissionError::EpochNotAdvanced {
            current: 31,
            proposed: 31,
        }
    );
    assert_eq!(owner.state(), TabletOwnerState::Serving);

    cleanup(&path);
}

#[test]
fn drain_epoch_is_immutable_once_handoff_begins() {
    let path = temp_wal("drain_immutable");
    cleanup(&path);

    let mut owner = TabletOwner::open(
        descriptor(37),
        &path,
        TabletOwnerConfig {
            queue_capacity: 2,
            ..Default::default()
        },
    )
    .unwrap();

    owner.begin_drain(38).unwrap();
    assert_eq!(
        owner.begin_drain(39).unwrap_err(),
        TabletAdmissionError::Draining { next_epoch: 38 }
    );
    assert_eq!(owner.state(), TabletOwnerState::Draining { next_epoch: 38 });

    cleanup(&path);
}

#[test]
fn owner_coalesces_fifo_prefix_up_to_write_cap() {
    let path = temp_wal("coalesce_write_cap");
    cleanup(&path);
    let mut owner = TabletOwner::open(
        descriptor(41),
        &path,
        TabletOwnerConfig {
            queue_capacity: 8,
            max_batch_writes: 2,
            max_batch_bytes: 1024 * 1024,
            max_batch_delay: std::time::Duration::from_micros(100),
        },
    )
    .unwrap();
    let first = owner.admit_write(41, 0, put(b"v1")).unwrap();
    let second = owner.admit_write(41, 1, put(b"v2")).unwrap();
    let third = owner.admit_write(41, 2, put(b"v3")).unwrap();
    let committed = owner.process_batch().unwrap();
    assert_eq!(
        committed.iter().map(|r| r.request_id).collect::<Vec<_>>(),
        vec![first, second]
    );
    assert_eq!(owner.queued_writes(), 1);
    assert_eq!(owner.process_batch().unwrap()[0].request_id, third);
    assert_eq!(owner.current_sequence().unwrap(), 3);
    cleanup(&path);
}

#[test]
fn owner_byte_cap_never_starves_oversized_head() {
    let path = temp_wal("coalesce_bytes");
    cleanup(&path);
    let mut owner = TabletOwner::open(
        descriptor(43),
        &path,
        TabletOwnerConfig {
            queue_capacity: 8,
            max_batch_writes: 8,
            max_batch_bytes: 12,
            max_batch_delay: std::time::Duration::from_micros(100),
        },
    )
    .unwrap();
    let first = owner
        .admit_write(43, 0, put(b"12345678901234567890"))
        .unwrap();
    let second = owner.admit_write(43, 1, put(b"x")).unwrap();
    let committed = owner.process_batch().unwrap();
    assert_eq!(committed.len(), 1);
    assert_eq!(committed[0].request_id, first);
    assert_eq!(owner.process_batch().unwrap()[0].request_id, second);
    cleanup(&path);
}
