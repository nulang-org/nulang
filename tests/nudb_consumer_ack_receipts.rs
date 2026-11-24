use nulang::database::retention::RetentionClass;
use nulang::database::retention_store::{DurableRetentionRegistry, RegistryError};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(1);
struct Sandbox(PathBuf);
impl Sandbox {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "nudb_consumer_ack_{}_{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn registry(&self) -> PathBuf {
        self.0.join("tablet.pins")
    }
}
impl Drop for Sandbox {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}
fn open(s: &Sandbox) -> DurableRetentionRegistry {
    DurableRetentionRegistry::open(s.registry(), 201, 4).unwrap()
}
fn certify(r: &mut DurableRetentionRegistry) {
    for class in [
        RetentionClass::Changefeed,
        RetentionClass::Backup,
        RetentionClass::Replica,
        RetentionClass::Branch,
    ] {
        r.certify_complete(class).unwrap();
    }
}

#[test]
fn two_phase_delivery_pins_old_cursor_across_restart_and_blocks_reclaim() {
    let s = Sandbox::new();
    {
        let mut r = open(&s);
        certify(&mut r);
        r.register_receipt_managed(RetentionClass::Changefeed, "sink", 3)
            .unwrap();
        r.begin_delivery(RetentionClass::Changefeed, "sink", 8, "delivery-1")
            .unwrap();
        assert_eq!(
            r.consumer_cursor(RetentionClass::Changefeed, "sink")
                .unwrap(),
            Some(3)
        );
        assert_eq!(
            r.delivery_intent(RetentionClass::Changefeed, "sink")
                .unwrap(),
            Some((8, "delivery-1".into()))
        );
        assert!(matches!(
            r.reserve_reclaim_through(3, 8),
            Err(RegistryError::ReclaimBlocked(_))
        ));
    }
    let mut restored = open(&s);
    assert_eq!(
        restored
            .consumer_cursor(RetentionClass::Changefeed, "sink")
            .unwrap(),
        Some(3)
    );
    assert_eq!(
        restored
            .delivery_intent(RetentionClass::Changefeed, "sink")
            .unwrap(),
        Some((8, "delivery-1".into()))
    );
    restored
        .begin_delivery(RetentionClass::Changefeed, "sink", 8, "delivery-1")
        .unwrap();
    assert!(matches!(
        restored.begin_delivery(RetentionClass::Changefeed, "sink", 9, "delivery-2"),
        Err(RegistryError::DeliveryInFlight)
    ));
}

#[test]
fn commit_after_sink_receipt_is_durable_and_only_then_advances_cursor() {
    let s = Sandbox::new();
    {
        let mut r = open(&s);
        certify(&mut r);
        r.register_receipt_managed(RetentionClass::Replica, "r1", 2)
            .unwrap();
        r.begin_delivery(RetentionClass::Replica, "r1", 6, "batch-17")
            .unwrap();
        assert_eq!(
            r.consumer_cursor(RetentionClass::Replica, "r1").unwrap(),
            Some(2)
        );
        r.commit_delivery(
            RetentionClass::Replica,
            "r1",
            6,
            "batch-17",
            b"durable-external-receipt-17",
        )
        .unwrap();
        assert_eq!(
            r.consumer_cursor(RetentionClass::Replica, "r1").unwrap(),
            Some(6)
        );
        assert_eq!(
            r.delivery_intent(RetentionClass::Replica, "r1").unwrap(),
            None
        );
    }
    let mut recovered = open(&s);
    assert_eq!(
        recovered
            .consumer_cursor(RetentionClass::Replica, "r1")
            .unwrap(),
        Some(6)
    );
    recovered
        .commit_delivery(
            RetentionClass::Replica,
            "r1",
            6,
            "batch-17",
            b"durable-external-receipt-17",
        )
        .unwrap();
    recovered.reserve_reclaim_through(5, 8).unwrap();
    assert_eq!(recovered.minimum_wal_cursor(), 6);
}

#[test]
fn receipt_managed_consumer_cannot_use_legacy_advance_or_release() {
    let s = Sandbox::new();
    let mut r = open(&s);
    r.register_receipt_managed(RetentionClass::Backup, "archive", 4)
        .unwrap();
    let before = fs::read(s.registry()).unwrap();
    assert!(matches!(
        r.advance(RetentionClass::Backup, "archive", 5),
        Err(RegistryError::ReceiptRequired)
    ));
    assert!(matches!(
        r.release(RetentionClass::Backup, "archive"),
        Err(RegistryError::ReceiptRequired)
    ));
    assert_eq!(fs::read(s.registry()).unwrap(), before);
    assert_eq!(
        r.consumer_cursor(RetentionClass::Backup, "archive")
            .unwrap(),
        Some(4)
    );
}

#[test]
fn invalid_or_wrong_delivery_receipts_do_not_advance_or_mutate_snapshot() {
    let s = Sandbox::new();
    let mut r = open(&s);
    r.register_receipt_managed(RetentionClass::Changefeed, "sink", 2)
        .unwrap();
    assert!(matches!(
        r.commit_delivery(RetentionClass::Changefeed, "sink", 4, "unknown", b"ack"),
        Err(RegistryError::NoDeliveryIntent)
    ));
    assert!(matches!(
        r.begin_delivery(RetentionClass::Changefeed, "sink", 1, "bad"),
        Err(RegistryError::CursorRegression)
    ));
    assert!(matches!(
        r.begin_delivery(RetentionClass::Changefeed, "sink", 4, ""),
        Err(RegistryError::InvalidReceipt)
    ));
    r.begin_delivery(RetentionClass::Changefeed, "sink", 4, "task-1")
        .unwrap();
    let bytes = fs::read(s.registry()).unwrap();
    for (to, id, ack) in [
        (5, "task-1", b"ack".as_slice()),
        (4, "other", b"ack".as_slice()),
        (4, "task-1", b"".as_slice()),
    ] {
        assert!(r
            .commit_delivery(RetentionClass::Changefeed, "sink", to, id, ack)
            .is_err());
        assert_eq!(fs::read(s.registry()).unwrap(), bytes);
        assert_eq!(
            r.consumer_cursor(RetentionClass::Changefeed, "sink")
                .unwrap(),
            Some(2)
        );
    }
}

#[test]
fn same_delivery_identifier_is_idempotent_and_conflicting_receipts_are_rejected() {
    let s = Sandbox::new();
    let mut r = open(&s);
    r.register_receipt_managed(RetentionClass::Branch, "branch", 2)
        .unwrap();
    r.begin_delivery(RetentionClass::Branch, "branch", 3, "step-a")
        .unwrap();
    let revision = r
        .commit_delivery(RetentionClass::Branch, "branch", 3, "step-a", b"receipt-a")
        .unwrap();
    let raw = fs::read(s.registry()).unwrap();
    assert_eq!(
        r.commit_delivery(RetentionClass::Branch, "branch", 3, "step-a", b"receipt-a")
            .unwrap(),
        revision
    );
    assert_eq!(fs::read(s.registry()).unwrap(), raw);
    assert!(matches!(
        r.commit_delivery(RetentionClass::Branch, "branch", 3, "step-a", b"receipt-b"),
        Err(RegistryError::ConflictingReceipt)
    ));
    assert!(matches!(
        r.begin_delivery(RetentionClass::Branch, "branch", 4, "step-a"),
        Err(RegistryError::ConflictingReceipt)
    ));
    assert_eq!(fs::read(s.registry()).unwrap(), raw);
}

#[test]
fn classic_registrations_stay_backwards_compatible() {
    let s = Sandbox::new();
    let mut r = open(&s);
    r.register(RetentionClass::Replica, "classic", 2).unwrap();
    r.advance(RetentionClass::Replica, "classic", 5).unwrap();
    r.release(RetentionClass::Replica, "classic").unwrap();
    assert_eq!(
        r.consumer_cursor(RetentionClass::Replica, "classic")
            .unwrap(),
        None
    );
}

#[test]
fn stale_receipt_enrollment_cannot_bypass_durable_floor() {
    let s = Sandbox::new();
    let mut r = open(&s);
    certify(&mut r);
    r.reserve_reclaim_through(6, 10).unwrap();
    assert!(matches!(
        r.register_receipt_managed(RetentionClass::Changefeed, "sink", 6),
        Err(RegistryError::HistoryUnavailable {
            earliest: 7,
            requested: 6
        })
    ));
    r.register_receipt_managed(RetentionClass::Changefeed, "sink", 7)
        .unwrap();
}

#[test]
fn crash_fixture_process_exits_after_persisted_delivery_intent() {
    let Ok(path) = std::env::var("NUDB_ACK_CRASH_REGISTRY") else {
        return;
    };
    let mut r = DurableRetentionRegistry::open(path, 201, 4).unwrap();
    r.register_receipt_managed(RetentionClass::Changefeed, "sink", 2)
        .unwrap();
    r.begin_delivery(RetentionClass::Changefeed, "sink", 5, "batch-crash")
        .unwrap();
    std::process::exit(27);
}

#[test]
fn abrupt_process_exit_before_external_ack_replays_same_delivery_safely() {
    use std::process::Command;
    let s = Sandbox::new();
    let status = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "crash_fixture_process_exits_after_persisted_delivery_intent",
        ])
        .env("NUDB_ACK_CRASH_REGISTRY", s.registry())
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(27));
    let mut r = open(&s);
    assert_eq!(
        r.consumer_cursor(RetentionClass::Changefeed, "sink")
            .unwrap(),
        Some(2)
    );
    assert_eq!(
        r.delivery_intent(RetentionClass::Changefeed, "sink")
            .unwrap(),
        Some((5, "batch-crash".to_string()))
    );
    r.begin_delivery(RetentionClass::Changefeed, "sink", 5, "batch-crash")
        .unwrap();
    r.commit_delivery(
        RetentionClass::Changefeed,
        "sink",
        5,
        "batch-crash",
        b"external-ack-after-replay",
    )
    .unwrap();
    drop(r);
    let r = open(&s);
    assert_eq!(
        r.consumer_cursor(RetentionClass::Changefeed, "sink")
            .unwrap(),
        Some(5)
    );
}
