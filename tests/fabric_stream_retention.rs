use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, UNIX_EPOCH};

use nulang::runtime::{FabricStreamConfig, FileFabricStreamStore};

fn test_dir(label: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "nulang-fabric-retention-{label}-{}-{id}",
        std::process::id()
    ))
}

fn one_record_segments() -> FabricStreamConfig {
    FabricStreamConfig {
        segment_max_bytes: 64,
    }
}

#[test]
fn sequence_retention_prunes_segments_and_survives_restart() {
    let root = test_dir("restart");
    {
        let mut store = FileFabricStreamStore::open(&root).unwrap();
        store.create_stream("events", one_record_segments()).unwrap();
        for byte in [1_u8, 2, 3, 4] {
            store.append("events", &[byte; 32]).unwrap();
        }

        let report = store.retain_from_sequence("events", 3).unwrap();
        assert_eq!(report.requested_first_sequence, 3);
        assert_eq!(report.effective_first_sequence, 3);
        assert_eq!(report.deleted_segments, 2);
        assert_eq!(report.deleted_records, 2);

        let info = store.stream_info("events").unwrap();
        assert_eq!(info.first_sequence, 3);
        assert_eq!(info.next_sequence, 5);
        assert_eq!(info.segment_count, 2);

        let retained = store.read_from("events", 1, 10).unwrap();
        assert_eq!(
            retained
                .iter()
                .map(|record| record.sequence)
                .collect::<Vec<_>>(),
            vec![3, 4]
        );
    }

    let mut reopened = FileFabricStreamStore::open(&root).unwrap();
    let info = reopened.stream_info("events").unwrap();
    assert_eq!(info.first_sequence, 3);
    assert_eq!(info.next_sequence, 5);
    assert_eq!(
        reopened
            .read_from("events", 1, 10)
            .unwrap()
            .iter()
            .map(|record| record.sequence)
            .collect::<Vec<_>>(),
        vec![3, 4]
    );
    assert_eq!(reopened.append("events", b"after-restart").unwrap(), 5);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn retention_rounds_down_to_the_containing_segment_boundary() {
    let root = test_dir("boundary");
    let mut store = FileFabricStreamStore::open(&root).unwrap();
    store
        .create_stream(
            "events",
            FabricStreamConfig {
                segment_max_bytes: 120,
            },
        )
        .unwrap();

    for byte in [1_u8, 2, 3, 4] {
        store.append("events", &[byte; 10]).unwrap();
    }

    let report = store.retain_from_sequence("events", 4).unwrap();
    assert_eq!(report.requested_first_sequence, 4);
    assert_eq!(report.effective_first_sequence, 3);
    assert_eq!(report.deleted_segments, 1);
    assert_eq!(report.deleted_records, 2);

    let retained = store.read_from("events", 1, 10).unwrap();
    assert_eq!(
        retained
            .iter()
            .map(|record| record.sequence)
            .collect::<Vec<_>>(),
        vec![3, 4]
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn retention_refuses_to_prune_past_a_named_consumer_cursor() {
    let root = test_dir("consumer");
    let mut store = FileFabricStreamStore::open(&root).unwrap();
    store.create_stream("events", one_record_segments()).unwrap();
    for byte in [1_u8, 2, 3] {
        store.append("events", &[byte; 32]).unwrap();
    }

    store.commit_cursor("events", "billing", 1).unwrap();

    let error = store.retain_from_sequence("events", 3).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert!(error.to_string().contains("billing"));
    assert_eq!(store.stream_info("events").unwrap().first_sequence, 1);
    assert_eq!(store.stream_info("events").unwrap().segment_count, 3);

    store.commit_cursor("events", "billing", 2).unwrap();
    let report = store.retain_from_sequence("events", 3).unwrap();
    assert_eq!(report.effective_first_sequence, 3);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn retention_refuses_to_prune_an_unacknowledged_delivery_without_a_cursor() {
    let root = test_dir("inflight");
    let mut store = FileFabricStreamStore::open(&root).unwrap();
    store.create_stream("events", one_record_segments()).unwrap();
    store.append("events", &[1_u8; 32]).unwrap();
    store.append("events", &[2_u8; 32]).unwrap();

    let deliveries = store
        .deliver_consumer_at(
            "events",
            "worker",
            1,
            Duration::from_secs(30),
            UNIX_EPOCH + Duration::from_secs(100),
        )
        .unwrap();
    assert_eq!(deliveries[0].record.sequence, 1);

    let error = store.retain_from_sequence("events", 2).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert!(error.to_string().contains("worker"));
    assert_eq!(store.stream_info("events").unwrap().first_sequence, 1);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn new_consumers_start_immediately_before_the_retained_floor() {
    let root = test_dir("new-consumer");
    let mut store = FileFabricStreamStore::open(&root).unwrap();
    store.create_stream("events", one_record_segments()).unwrap();
    for byte in [1_u8, 2, 3] {
        store.append("events", &[byte; 32]).unwrap();
    }
    store.retain_from_sequence("events", 3).unwrap();

    assert_eq!(store.cursor("events", "fresh-worker").unwrap(), 2);
    let deliveries = store
        .deliver_consumer_at(
            "events",
            "fresh-worker",
            1,
            Duration::from_secs(30),
            UNIX_EPOCH + Duration::from_secs(100),
        )
        .unwrap();
    assert_eq!(deliveries[0].record.sequence, 3);

    store
        .ack_consumer("events", "fresh-worker", 3)
        .unwrap();
    assert_eq!(store.cursor("events", "fresh-worker").unwrap(), 3);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn retention_can_prune_all_history_without_reusing_sequence_numbers() {
    let root = test_dir("all");
    {
        let mut store = FileFabricStreamStore::open(&root).unwrap();
        store.create_stream("events", one_record_segments()).unwrap();
        for byte in [1_u8, 2, 3] {
            store.append("events", &[byte; 32]).unwrap();
        }

        let report = store.retain_from_sequence("events", 4).unwrap();
        assert_eq!(report.effective_first_sequence, 4);
        assert_eq!(report.deleted_segments, 3);
        assert_eq!(report.deleted_records, 3);

        let info = store.stream_info("events").unwrap();
        assert_eq!(info.first_sequence, 4);
        assert_eq!(info.next_sequence, 4);
        assert_eq!(info.segment_count, 0);
        assert!(store.read_from("events", 1, 10).unwrap().is_empty());
    }

    let mut reopened = FileFabricStreamStore::open(&root).unwrap();
    assert_eq!(reopened.stream_info("events").unwrap().first_sequence, 4);
    assert_eq!(reopened.append("events", b"new").unwrap(), 4);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn retention_rejects_a_floor_beyond_the_next_sequence() {
    let root = test_dir("invalid");
    let mut store = FileFabricStreamStore::open(&root).unwrap();
    store.create_stream("events", one_record_segments()).unwrap();
    store.append("events", &[1_u8; 32]).unwrap();

    let error = store.retain_from_sequence("events", 3).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(store.stream_info("events").unwrap().first_sequence, 1);

    let _ = fs::remove_dir_all(root);
}
