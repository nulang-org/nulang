use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, UNIX_EPOCH};

use nulang::runtime::{FabricStreamConfig, FileFabricStreamStore};

fn test_dir(label: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "nulang-fabric-consumer-ack-{label}-{}-{id}",
        std::process::id()
    ))
}

#[test]
fn acknowledgements_only_advance_the_cursor_contiguously() {
    let root = test_dir("contiguous");
    let mut store = FileFabricStreamStore::open(&root).unwrap();
    store
        .create_stream("events", FabricStreamConfig::default())
        .unwrap();
    for payload in [b"a".as_slice(), b"b".as_slice(), b"c".as_slice()] {
        store.append("events", payload).unwrap();
    }

    let delivered = store
        .deliver_consumer_at(
            "events",
            "worker",
            3,
            Duration::from_secs(30),
            UNIX_EPOCH + Duration::from_secs(100),
        )
        .unwrap();
    assert_eq!(
        delivered
            .iter()
            .map(|delivery| delivery.record.sequence)
            .collect::<Vec<_>>(),
        vec![1, 2, 3]
    );

    store.ack_consumer("events", "worker", 2).unwrap();
    assert_eq!(store.cursor("events", "worker").unwrap(), 0);

    store.ack_consumer("events", "worker", 1).unwrap();
    assert_eq!(store.cursor("events", "worker").unwrap(), 2);

    store.ack_consumer("events", "worker", 3).unwrap();
    assert_eq!(store.cursor("events", "worker").unwrap(), 3);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn nack_makes_a_delivery_immediately_eligible_for_redelivery() {
    let root = test_dir("nack");
    let mut store = FileFabricStreamStore::open(&root).unwrap();
    store
        .create_stream("events", FabricStreamConfig::default())
        .unwrap();
    store.append("events", b"a").unwrap();

    let first = store
        .deliver_consumer_at(
            "events",
            "worker",
            1,
            Duration::from_secs(30),
            UNIX_EPOCH + Duration::from_secs(100),
        )
        .unwrap();
    assert_eq!(first[0].attempt, 1);
    assert!(!first[0].redelivered);

    store.nack_consumer("events", "worker", 1).unwrap();

    let second = store
        .deliver_consumer_at(
            "events",
            "worker",
            1,
            Duration::from_secs(30),
            UNIX_EPOCH + Duration::from_secs(101),
        )
        .unwrap();
    assert_eq!(second[0].record.sequence, 1);
    assert_eq!(second[0].attempt, 2);
    assert!(second[0].redelivered);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn delivery_leases_survive_restart_and_redeliver_after_deadline() {
    let root = test_dir("restart");
    {
        let mut store = FileFabricStreamStore::open(&root).unwrap();
        store
            .create_stream("events", FabricStreamConfig::default())
            .unwrap();
        store.append("events", b"a").unwrap();

        let first = store
            .deliver_consumer_at(
                "events",
                "worker",
                1,
                Duration::from_secs(10),
                UNIX_EPOCH + Duration::from_secs(100),
            )
            .unwrap();
        assert_eq!(first[0].attempt, 1);
    }

    let mut reopened = FileFabricStreamStore::open(&root).unwrap();
    let before_deadline = reopened
        .deliver_consumer_at(
            "events",
            "worker",
            1,
            Duration::from_secs(10),
            UNIX_EPOCH + Duration::from_secs(105),
        )
        .unwrap();
    assert!(before_deadline.is_empty());

    let redelivered = reopened
        .deliver_consumer_at(
            "events",
            "worker",
            1,
            Duration::from_secs(10),
            UNIX_EPOCH + Duration::from_secs(111),
        )
        .unwrap();
    assert_eq!(redelivered[0].record.sequence, 1);
    assert_eq!(redelivered[0].attempt, 2);
    assert!(redelivered[0].redelivered);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn active_leases_do_not_block_delivery_of_newer_records() {
    let root = test_dir("nonblocking");
    let mut store = FileFabricStreamStore::open(&root).unwrap();
    store
        .create_stream("events", FabricStreamConfig::default())
        .unwrap();
    store.append("events", b"a").unwrap();
    store.append("events", b"b").unwrap();

    let first = store
        .deliver_consumer_at(
            "events",
            "worker",
            1,
            Duration::from_secs(30),
            UNIX_EPOCH + Duration::from_secs(100),
        )
        .unwrap();
    assert_eq!(first[0].record.sequence, 1);

    let second = store
        .deliver_consumer_at(
            "events",
            "worker",
            1,
            Duration::from_secs(30),
            UNIX_EPOCH + Duration::from_secs(101),
        )
        .unwrap();
    assert_eq!(second[0].record.sequence, 2);
    let _ = fs::remove_dir_all(root);
}
