use nulang::database::consumer_sink::{DurableFileSink, SinkError};
use nulang::database::retention::RetentionClass;
use nulang::database::retention_store::DurableRetentionRegistry;
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
            "nudb_sink_{}_{}",
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
        fs::remove_dir_all(&self.0).unwrap();
    }
}
fn reg(s: &Sandbox) -> DurableRetentionRegistry {
    DurableRetentionRegistry::open(s.registry(), 201, 4).unwrap()
}
fn count_effect_files(s: &Sandbox) -> usize {
    fs::read_dir(s.sink())
        .unwrap()
        .map(Result::unwrap)
        .filter(|e| e.file_name().to_string_lossy().starts_with("effect-"))
        .count()
}
#[test]
fn committed_file_effect_and_receipt_survive_restart_without_duplicate_side_effect() {
    let s = Sandbox::new();
    {
        let mut r = reg(&s);
        r.register_receipt_managed(RetentionClass::Changefeed, "out", 2)
            .unwrap();
        let sink = DurableFileSink::open(s.sink()).unwrap();
        sink.deliver_and_ack(
            &mut r,
            RetentionClass::Changefeed,
            "out",
            5,
            "batch-1",
            b"durable-message",
        )
        .unwrap();
        assert_eq!(
            r.consumer_cursor(RetentionClass::Changefeed, "out")
                .unwrap(),
            Some(5)
        );
        assert_eq!(
            sink.read_effect(RetentionClass::Changefeed, "out", "batch-1")
                .unwrap(),
            Some(b"durable-message".to_vec())
        );
        assert_eq!(count_effect_files(&s), 1);
    }
    let mut r = reg(&s);
    let sink = DurableFileSink::open(s.sink()).unwrap();
    assert_eq!(
        sink.read_effect(RetentionClass::Changefeed, "out", "batch-1")
            .unwrap(),
        Some(b"durable-message".to_vec())
    );
    assert_eq!(
        r.consumer_cursor(RetentionClass::Changefeed, "out")
            .unwrap(),
        Some(5)
    );
    // Replays use stable immutable sink records; local acknowledged replay is idempotent.
    let receipt = sink
        .apply(
            RetentionClass::Changefeed,
            "out",
            5,
            "batch-1",
            b"durable-message",
        )
        .unwrap();
    assert_eq!(
        r.commit_delivery(RetentionClass::Changefeed, "out", 5, "batch-1", &receipt)
            .unwrap(),
        r.revision()
    );
    assert_eq!(count_effect_files(&s), 1);
}
#[test]
fn conflicting_replay_fails_before_advancing_pin() {
    let s = Sandbox::new();
    let mut r = reg(&s);
    r.register_receipt_managed(RetentionClass::Replica, "sink", 2)
        .unwrap();
    let sink = DurableFileSink::open(s.sink()).unwrap();
    r.begin_delivery(RetentionClass::Replica, "sink", 4, "job-1")
        .unwrap();
    sink.apply(RetentionClass::Replica, "sink", 4, "job-1", b"original")
        .unwrap();
    assert!(matches!(
        sink.apply(RetentionClass::Replica, "sink", 4, "job-1", b"mutated"),
        Err(SinkError::Conflict)
    ));
    assert!(matches!(
        sink.apply(RetentionClass::Replica, "sink", 5, "job-1", b"original"),
        Err(SinkError::Conflict)
    ));
    assert_eq!(
        r.consumer_cursor(RetentionClass::Replica, "sink").unwrap(),
        Some(2)
    );
    assert_eq!(
        sink.read_effect(RetentionClass::Replica, "sink", "job-1")
            .unwrap(),
        Some(b"original".to_vec())
    );
}
#[test]
fn corrupt_immutable_sink_record_blocks_acknowledgment() {
    let s = Sandbox::new();
    let mut r = reg(&s);
    r.register_receipt_managed(RetentionClass::Backup, "archive", 2)
        .unwrap();
    let sink = DurableFileSink::open(s.sink()).unwrap();
    r.begin_delivery(RetentionClass::Backup, "archive", 5, "batch")
        .unwrap();
    sink.apply(RetentionClass::Backup, "archive", 5, "batch", b"saved")
        .unwrap();
    drop(sink);
    let effect = fs::read_dir(s.sink())
        .unwrap()
        .map(Result::unwrap)
        .find(|e| e.file_name().to_string_lossy().starts_with("effect-"))
        .unwrap()
        .path();
    let mut bytes = fs::read(&effect).unwrap();
    *bytes.last_mut().unwrap() ^= 0x40;
    fs::write(effect, bytes).unwrap();
    let sink = DurableFileSink::open(s.sink()).unwrap();
    assert!(matches!(
        sink.apply(RetentionClass::Backup, "archive", 5, "batch", b"saved"),
        Err(SinkError::Corrupt)
    ));
    assert_eq!(
        r.consumer_cursor(RetentionClass::Backup, "archive")
            .unwrap(),
        Some(2)
    );
}
#[test]
fn missing_effect_or_pending_delivery_cannot_be_skipped() {
    let s = Sandbox::new();
    let mut r = reg(&s);
    r.register_receipt_managed(RetentionClass::Changefeed, "sink", 3)
        .unwrap();
    let sink = DurableFileSink::open(s.sink()).unwrap();
    assert!(sink
        .deliver_and_ack(
            &mut r,
            RetentionClass::Changefeed,
            "sink",
            3,
            "zero",
            b"none"
        )
        .is_err());
    assert_eq!(count_effect_files(&s), 0);
    r.begin_delivery(RetentionClass::Changefeed, "sink", 8, "first")
        .unwrap();
    assert!(sink
        .deliver_and_ack(
            &mut r,
            RetentionClass::Changefeed,
            "sink",
            9,
            "second",
            b"data"
        )
        .is_err());
    assert_eq!(count_effect_files(&s), 0);
    assert_eq!(
        r.consumer_cursor(RetentionClass::Changefeed, "sink")
            .unwrap(),
        Some(3)
    );
}
#[test]
fn invalid_effect_is_rejected_before_persisting_any_delivery_intent() {
    let s = Sandbox::new();
    let mut r = reg(&s);
    r.register_receipt_managed(RetentionClass::Changefeed, "sink", 2)
        .unwrap();
    let sink = DurableFileSink::open(s.sink()).unwrap();
    let invalid = vec![b'x'; 64 * 1024 + 1];
    assert!(matches!(
        sink.deliver_and_ack(
            &mut r,
            RetentionClass::Changefeed,
            "sink",
            4,
            "oversized",
            &invalid
        ),
        Err(SinkError::InvalidDelivery)
    ));
    assert_eq!(
        r.delivery_intent(RetentionClass::Changefeed, "sink")
            .unwrap(),
        None
    );
    assert_eq!(
        r.consumer_cursor(RetentionClass::Changefeed, "sink")
            .unwrap(),
        Some(2)
    );
    assert_eq!(count_effect_files(&s), 0);
}

#[test]
fn sibling_sink_owner_excluded_and_reacquired_after_drop() {
    let s = Sandbox::new();
    let sink = DurableFileSink::open(s.sink()).unwrap();
    assert!(matches!(
        DurableFileSink::open(s.sink()),
        Err(SinkError::WriterLocked)
    ));
    drop(sink);
    // Do not retry or sleep: a dropped cooperative owner must release the lock.
    let reopened = DurableFileSink::open(s.sink()).unwrap();
    assert!(matches!(
        DurableFileSink::open(s.sink()),
        Err(SinkError::WriterLocked)
    ));
    drop(reopened);
}

#[cfg(unix)]
#[test]
fn symlinked_effect_is_rejected_without_modifying_target() {
    use std::os::unix::fs::symlink;

    let s = Sandbox::new();
    let outside = s.0.join("outside");
    fs::write(&outside, b"unchanged").unwrap();
    let record_name = DurableFileSink::record_name(RetentionClass::Replica, "replica", "evil");
    symlink(&outside, s.sink().join(record_name)).unwrap();

    let sink = DurableFileSink::open(s.sink()).unwrap();
    assert!(sink
        .apply(RetentionClass::Replica, "replica", 5, "evil", b"data")
        .is_err());
    assert_eq!(fs::read(outside).unwrap(), b"unchanged");
}
#[cfg(unix)]
#[test]
fn subprocess_sink_owner_lock_fixture() {
    use std::io::{Read, Write};

    let Ok(root) = std::env::var("NUDB_SINK_OWNER_LOCK_ROOT") else {
        return;
    };
    let sink = DurableFileSink::open(PathBuf::from(root).join("effects")).unwrap();
    println!("NUDB_SINK_LOCK_READY");
    std::io::stdout().flush().unwrap();

    // Parent releases this process through a pipe, not a timing-based sleep.
    let mut release = [0_u8; 1];
    std::io::stdin().read_exact(&mut release).unwrap();
    assert_eq!(release, [b'R']);
    drop(sink);
}

#[cfg(unix)]
#[test]
fn cross_process_sink_owner_lock_is_exclusive_and_released_after_exit() {
    use std::io::{BufRead, BufReader, Write};
    use std::process::{Command, Stdio};

    let s = Sandbox::new();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "subprocess_sink_owner_lock_fixture",
            "--nocapture",
        ])
        .env("NUDB_SINK_OWNER_LOCK_ROOT", &s.0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();

    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    let mut acquired = false;
    while stdout.read_line(&mut line).unwrap() != 0 {
        if line.contains("NUDB_SINK_LOCK_READY") {
            acquired = true;
            break;
        }
        line.clear();
    }
    assert!(acquired, "child exited before acquiring its sink lock");
    assert!(matches!(
        DurableFileSink::open(s.sink()),
        Err(SinkError::WriterLocked)
    ));

    child.stdin.take().unwrap().write_all(b"R").unwrap();
    assert!(child.wait().unwrap().success());

    // The child has exited and released its OS lock; fail rather than retry.
    let reopened = DurableFileSink::open(s.sink()).unwrap();
    assert!(matches!(
        DurableFileSink::open(s.sink()),
        Err(SinkError::WriterLocked)
    ));
    drop(reopened);
}

#[test]
fn subprocess_fault_fixture() {
    let Ok(parent) = std::env::var("NUDB_SINK_CRASH_ROOT") else {
        return;
    };
    let stage = std::env::var("NUDB_SINK_CRASH_STAGE").unwrap();
    let p = PathBuf::from(parent);
    let mut r = DurableRetentionRegistry::open(p.join("tablet.pins"), 201, 4).unwrap();
    r.register_receipt_managed(RetentionClass::Changefeed, "s", 2)
        .unwrap();
    let sink = DurableFileSink::open(p.join("effects")).unwrap();
    r.begin_delivery(RetentionClass::Changefeed, "s", 5, "exit-1")
        .unwrap();
    if stage == "intent" {
        std::process::exit(31);
    }
    sink.apply(RetentionClass::Changefeed, "s", 5, "exit-1", b"persisted")
        .unwrap();
    std::process::exit(32);
}
#[test]
fn crash_after_intent_or_after_effect_replays_without_advancing_early() {
    use std::process::Command;
    for (stage, code) in [("intent", 31), ("effect", 32)] {
        let s = Sandbox::new();
        let status = Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("subprocess_fault_fixture")
            .env("NUDB_SINK_CRASH_ROOT", &s.0)
            .env("NUDB_SINK_CRASH_STAGE", stage)
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(code));
        let mut r = reg(&s);
        assert_eq!(
            r.consumer_cursor(RetentionClass::Changefeed, "s").unwrap(),
            Some(2)
        );
        assert_eq!(
            r.delivery_intent(RetentionClass::Changefeed, "s").unwrap(),
            Some((5, "exit-1".into()))
        );
        let sink = DurableFileSink::open(s.sink()).unwrap();
        sink.deliver_and_ack(
            &mut r,
            RetentionClass::Changefeed,
            "s",
            5,
            "exit-1",
            b"persisted",
        )
        .unwrap();
        assert_eq!(
            r.consumer_cursor(RetentionClass::Changefeed, "s").unwrap(),
            Some(5)
        );
        assert_eq!(count_effect_files(&s), 1);
    }
}
