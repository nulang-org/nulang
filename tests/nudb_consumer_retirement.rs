use nulang::database::consumer_sink::{DurableFileSink, SinkError};
use nulang::database::retention::RetentionClass;
use nulang::database::retention_store::{DurableRetentionRegistry, RegistryError};
use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};
static NEXT: AtomicU64 = AtomicU64::new(1);
struct Sandbox(PathBuf);
impl Sandbox {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "nudb_retirement_{}_{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&p).unwrap();
        fs::create_dir(p.join("effects")).unwrap();
        Self(p)
    }
    fn registry(&self) -> PathBuf {
        self.0.join("tablet.pins")
    }
    fn sink(&self) -> PathBuf {
        self.0.join("effects")
    }
}
impl Drop for Sandbox {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap()
    }
}
fn registry(s: &Sandbox) -> DurableRetentionRegistry {
    DurableRetentionRegistry::open(s.registry(), 201, 4).unwrap()
}
#[test]
fn local_closure_durably_retires_pin_and_prevents_id_reuse_after_restart() {
    let s = Sandbox::new();
    {
        let mut r = registry(&s);
        let sink = DurableFileSink::open(s.sink()).unwrap();
        r.register_receipt_managed(RetentionClass::Changefeed, "s1", 2)
            .unwrap();
        sink.deliver_and_ack(
            &mut r,
            RetentionClass::Changefeed,
            "s1",
            5,
            "batch-1",
            b"first",
        )
        .unwrap();
        sink.retire_consumer(&mut r, RetentionClass::Changefeed, "s1", "retire-2026")
            .unwrap();
        assert_eq!(
            r.consumer_cursor(RetentionClass::Changefeed, "s1").unwrap(),
            None
        );
        assert_eq!(
            r.consumer_retirement(RetentionClass::Changefeed, "s1")
                .unwrap()
                .unwrap()
                .0,
            5
        );
        assert!(matches!(
            sink.apply(RetentionClass::Changefeed, "s1", 6, "batch-2", b"later"),
            Err(SinkError::Retired)
        ));
        assert!(matches!(
            r.register_receipt_managed(RetentionClass::Changefeed, "s1", 6),
            Err(RegistryError::RetiredConsumer)
        ));
        assert!(matches!(
            r.register(RetentionClass::Changefeed, "s1", 6),
            Err(RegistryError::RetiredConsumer)
        ));
    }
    let mut r = registry(&s);
    let sink = DurableFileSink::open(s.sink()).unwrap();
    assert_eq!(
        r.consumer_cursor(RetentionClass::Changefeed, "s1").unwrap(),
        None
    );
    let before = fs::read(s.registry()).unwrap();
    sink.retire_consumer(&mut r, RetentionClass::Changefeed, "s1", "retire-2026")
        .unwrap();
    assert_eq!(fs::read(s.registry()).unwrap(), before);
    assert!(matches!(
        sink.deliver_and_ack(&mut r, RetentionClass::Changefeed, "s1", 6, "new", b"n"),
        Err(_)
    ));
}
#[test]
fn in_flight_delivery_cannot_be_retired_or_closed() {
    let s = Sandbox::new();
    let mut r = registry(&s);
    r.register_receipt_managed(RetentionClass::Replica, "replica", 2)
        .unwrap();
    r.begin_delivery(RetentionClass::Replica, "replica", 6, "pending")
        .unwrap();
    let sink = DurableFileSink::open(s.sink()).unwrap();
    let before = fs::read(s.registry()).unwrap();
    assert!(matches!(
        sink.retire_consumer(&mut r, RetentionClass::Replica, "replica", "close"),
        Err(SinkError::Registry(RegistryError::DeliveryInFlight))
    ));
    assert!(matches!(
        sink.seal_consumer(&r, RetentionClass::Replica, "replica", "close"),
        Err(SinkError::Registry(RegistryError::DeliveryInFlight))
    ));
    assert_eq!(fs::read(s.registry()).unwrap(), before);
    assert_eq!(
        r.consumer_cursor(RetentionClass::Replica, "replica")
            .unwrap(),
        Some(2)
    );
}
#[test]
fn crash_after_durable_closure_before_registry_unpin_keeps_pin_then_replays() {
    let s = Sandbox::new();
    {
        let mut r = registry(&s);
        r.register_receipt_managed(RetentionClass::Backup, "archive", 3)
            .unwrap();
        let sink = DurableFileSink::open(s.sink()).unwrap();
        sink.seal_consumer(&r, RetentionClass::Backup, "archive", "close-1")
            .unwrap();
        assert_eq!(
            r.consumer_cursor(RetentionClass::Backup, "archive")
                .unwrap(),
            Some(3)
        );
    }
    let mut r = registry(&s);
    assert_eq!(
        r.consumer_cursor(RetentionClass::Backup, "archive")
            .unwrap(),
        Some(3)
    );
    let sink = DurableFileSink::open(s.sink()).unwrap();
    assert!(matches!(
        sink.apply(RetentionClass::Backup, "archive", 4, "late", b"x"),
        Err(SinkError::Retired)
    ));
    sink.retire_consumer(&mut r, RetentionClass::Backup, "archive", "close-1")
        .unwrap();
    assert_eq!(
        r.consumer_cursor(RetentionClass::Backup, "archive")
            .unwrap(),
        None
    );
}
#[test]
fn conflicting_closure_id_and_corrupt_record_refuse_unpin() {
    let s = Sandbox::new();
    let mut r = registry(&s);
    r.register_receipt_managed(RetentionClass::Branch, "branch", 2)
        .unwrap();
    let sink = DurableFileSink::open(s.sink()).unwrap();
    sink.seal_consumer(&r, RetentionClass::Branch, "branch", "first")
        .unwrap();
    assert!(matches!(
        sink.retire_consumer(&mut r, RetentionClass::Branch, "branch", "second"),
        Err(SinkError::Conflict)
    ));
    assert_eq!(
        r.consumer_cursor(RetentionClass::Branch, "branch").unwrap(),
        Some(2)
    );
    drop(sink);
    let record = fs::read_dir(s.sink())
        .unwrap()
        .map(Result::unwrap)
        .find(|v| v.file_name().to_string_lossy().starts_with("retired-"))
        .unwrap()
        .path();
    let mut bytes = fs::read(&record).unwrap();
    *bytes.last_mut().unwrap() ^= 0x33;
    fs::write(&record, bytes).unwrap();
    let sink = DurableFileSink::open(s.sink()).unwrap();
    assert!(matches!(
        sink.retire_consumer(&mut r, RetentionClass::Branch, "branch", "first"),
        Err(SinkError::Corrupt)
    ));
    assert_eq!(
        r.consumer_cursor(RetentionClass::Branch, "branch").unwrap(),
        Some(2)
    );
}
#[test]
fn classic_and_absent_consumers_cannot_invoke_retirement() {
    let s = Sandbox::new();
    let mut r = registry(&s);
    r.register(RetentionClass::Backup, "classic", 2).unwrap();
    let sink = DurableFileSink::open(s.sink()).unwrap();
    assert!(matches!(
        sink.retire_consumer(&mut r, RetentionClass::Backup, "classic", "close"),
        Err(SinkError::Registry(RegistryError::ReceiptRequired))
    ));
    assert!(sink
        .retire_consumer(&mut r, RetentionClass::Backup, "missing", "close")
        .is_err());
    assert_eq!(
        r.consumer_cursor(RetentionClass::Backup, "classic")
            .unwrap(),
        Some(2)
    );
}
#[test]
fn termination_crash_fixture() {
    let Ok(root) = std::env::var("NUDB_TERMINATE_ROOT") else {
        return;
    };
    let p = PathBuf::from(root);
    let mut r = DurableRetentionRegistry::open(p.join("tablet.pins"), 201, 4).unwrap();
    r.register_receipt_managed(RetentionClass::Changefeed, "s", 2)
        .unwrap();
    let sink = DurableFileSink::open(p.join("effects")).unwrap();
    sink.seal_consumer(&r, RetentionClass::Changefeed, "s", "stop")
        .unwrap();
    std::process::exit(52);
}
#[test]
fn subprocess_exit_after_durable_sink_closure_retains_pin_until_reconciliation() {
    use std::process::Command;
    let s = Sandbox::new();
    let exit = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "termination_crash_fixture"])
        .env("NUDB_TERMINATE_ROOT", &s.0)
        .status()
        .unwrap();
    assert_eq!(exit.code(), Some(52));
    let mut r = registry(&s);
    assert_eq!(
        r.consumer_cursor(RetentionClass::Changefeed, "s").unwrap(),
        Some(2)
    );
    let sink = DurableFileSink::open(s.sink()).unwrap();
    sink.retire_consumer(&mut r, RetentionClass::Changefeed, "s", "stop")
        .unwrap();
    assert_eq!(
        r.consumer_cursor(RetentionClass::Changefeed, "s").unwrap(),
        None
    );
    assert!(matches!(
        r.register_receipt_managed(RetentionClass::Changefeed, "s", 2),
        Err(RegistryError::RetiredConsumer)
    ));
}
