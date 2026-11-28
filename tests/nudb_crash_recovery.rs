use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use nulang::database::store::{WalBackedError, WalBackedTablet};
use nulang::database::tablet::{KeyRange, TabletDescriptor, TabletId, TabletMutation};
use nulang::database::wal::WalError;

static NEXT_TEST: AtomicU64 = AtomicU64::new(1);

fn temp_wal(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "nulang_nudb_crash_{name}_{}_{}.wal",
        std::process::id(),
        NEXT_TEST.fetch_add(1, Ordering::Relaxed)
    ))
}

fn descriptor() -> TabletDescriptor {
    TabletDescriptor::new(
        TabletId::new(81).unwrap(),
        KeyRange::new(b"a".to_vec(), Some(b"z".to_vec())).unwrap(),
        5,
    )
    .unwrap()
}

fn ack_path(wal_path: &Path) -> PathBuf {
    wal_path.with_extension("ack")
}

fn cleanup(wal_path: &Path) {
    let _ = fs::remove_file(wal_path);
    let _ = fs::remove_file(wal_path.with_extension("checkpoint"));
    let _ = fs::remove_file(ack_path(wal_path));
}

fn commit_put(tablet: &mut WalBackedTablet, key: &[u8], value: &[u8]) {
    let sequence = tablet.current_sequence();
    let write = tablet
        .prepare_write(
            5,
            sequence,
            vec![TabletMutation::Put {
                key: key.to_vec(),
                value: value.to_vec(),
            }],
        )
        .unwrap();
    tablet.commit(write).unwrap();
}

// Own every subprocess until it is explicitly reaped, including unwinding.
struct FixtureChild(Child);

impl Drop for FixtureChild {
    fn drop(&mut self) {
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

fn wait_for_ack(child: &mut Child, path: &Path, timeout: Duration) -> io::Result<String> {
    let started = Instant::now();
    let result = (|| loop {
        match fs::read_to_string(path) {
            Ok(value) if value.starts_with("ACK ") => return Ok(value),
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid fixture acknowledgement",
                ))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        if let Some(status) = child.try_wait()? {
            return Err(io::Error::other(format!(
                "fixture exited before acknowledgement: {status}"
            )));
        }
        if started.elapsed() >= timeout {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "fixture did not publish a durability acknowledgement",
            ));
        }
        thread::sleep(Duration::from_millis(5).min(timeout.saturating_sub(started.elapsed())));
    })();
    if result.is_err() {
        // Always wait, even if kill races with natural exit.
        let _ = child.kill();
        child.wait()?;
    }
    result
}

fn spawn_until_ack(wal_path: &Path, action: &str) -> (FixtureChild, String) {
    let ack_path = ack_path(wal_path);
    let _ = fs::remove_file(&ack_path);

    let mut child = FixtureChild(
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "crash_fixture_child", "--ignored", "--nocapture"])
            .env("NULANG_NUDB_CRASH_CHILD_WAL", wal_path)
            .env("NULANG_NUDB_CRASH_CHILD_ACK", &ack_path)
            .env("NULANG_NUDB_CRASH_CHILD_ACTION", action)
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    let ack = wait_for_ack(&mut child.0, &ack_path, Duration::from_secs(5)).unwrap();
    (child, ack)
}

fn run_until_ack_then_kill(wal_path: &Path, action: &str) -> String {
    let (mut child, ack) = spawn_until_ack(wal_path, action);
    child.0.kill().unwrap();
    let status = child.0.wait().unwrap();
    assert!(!status.success(), "fixture unexpectedly exited gracefully");
    ack
}

#[test]
#[ignore = "subprocess fixture; invoked explicitly by parent recovery tests"]
fn crash_fixture_child() {
    let Some(wal_path) = std::env::var_os("NULANG_NUDB_CRASH_CHILD_WAL") else {
        return;
    };
    let action = std::env::var("NULANG_NUDB_CRASH_CHILD_ACTION")
        .expect("crash fixture action must be supplied by parent");
    let ack_path = std::env::var_os("NULANG_NUDB_CRASH_CHILD_ACK")
        .expect("crash fixture ack path must be supplied by parent");

    // Keep the writer alive through acknowledgement for the cross-process
    // exclusion fixture. Other crash fixtures still drop it before ACK.
    let mut _held_writer = None;
    #[cfg(unix)]
    let mut _held_managed = None;
    let acknowledgement = match action.as_str() {
        "never-ack" => loop {
            std::thread::park();
        },
        "hold-lock" => {
            let mut writer = WalBackedTablet::open(descriptor(), &wal_path).unwrap();
            commit_put(&mut writer, b"k", b"from-child");
            writer.checkpoint().unwrap();
            _held_writer = Some(writer);
            "ACK WRITER_LOCK 1".to_string()
        }
        "commit" => {
            let mut tablet = WalBackedTablet::open(descriptor(), &wal_path).unwrap();
            let sequence = tablet.current_sequence();
            let write = tablet
                .prepare_write(
                    5,
                    sequence,
                    vec![TabletMutation::Put {
                        key: b"k".to_vec(),
                        value: b"value".to_vec(),
                    }],
                )
                .unwrap();
            let committed = tablet.commit(write).unwrap();
            format!("ACK COMMIT {committed}")
        }
        "publish-checkpoint" => {
            let tablet = WalBackedTablet::open(descriptor(), &wal_path).unwrap();
            let sequence = tablet.current_sequence();
            tablet.publish_checkpoint().unwrap();
            format!("ACK CHECKPOINT {sequence}")
        }
        "checkpoint" => {
            let mut tablet = WalBackedTablet::open(descriptor(), &wal_path).unwrap();
            let sequence = tablet.current_sequence();
            tablet.checkpoint().unwrap();
            format!("ACK CHECKPOINT_RECLAIMED {sequence}")
        }
        #[cfg(unix)]
        "managed-enroll" | "managed-rotate-backup" => {
            let (managed, ack) = managed_child(Path::new(&wal_path), &action);
            _held_managed = Some(managed);
            ack
        }
        other => panic!("unknown crash fixture action: {other}"),
    };

    let ack_path = PathBuf::from(ack_path);
    let mut temp = ack_path.as_os_str().to_os_string();
    temp.push(".tmp");
    let temp = PathBuf::from(temp);
    let mut file = fs::File::create(&temp).unwrap();
    file.write_all(acknowledgement.as_bytes()).unwrap();
    file.write_all(b"\n").unwrap();
    file.sync_data().unwrap();
    drop(file);
    fs::rename(&temp, &ack_path).unwrap();

    loop {
        std::thread::park();
    }
}

#[test]
fn acknowledged_commit_survives_immediate_process_kill() {
    let wal_path = temp_wal("commit_ack");
    cleanup(&wal_path);

    let ack = run_until_ack_then_kill(&wal_path, "commit");
    assert_eq!(ack.trim(), "ACK COMMIT 1");

    let tablet = WalBackedTablet::open(descriptor(), &wal_path).unwrap();
    assert_eq!(tablet.current_sequence(), 1);
    assert_eq!(tablet.read_latest(b"k"), Some(&b"value"[..]));

    cleanup(&wal_path);
}

#[test]
fn published_checkpoint_survives_kill_before_reclamation() {
    let wal_path = temp_wal("checkpoint_publish");
    cleanup(&wal_path);

    {
        let mut tablet = WalBackedTablet::open(descriptor(), &wal_path).unwrap();
        commit_put(&mut tablet, b"k", b"v1");
        commit_put(&mut tablet, b"k", b"v2");
    }

    let ack = run_until_ack_then_kill(&wal_path, "publish-checkpoint");
    assert_eq!(ack.trim(), "ACK CHECKPOINT 2");

    let tablet = WalBackedTablet::open(descriptor(), &wal_path).unwrap();
    assert_eq!(tablet.current_sequence(), 2);
    assert_eq!(tablet.read_at(b"k", 1).unwrap(), Some(&b"v1"[..]));
    assert_eq!(tablet.read_at(b"k", 2).unwrap(), Some(&b"v2"[..]));

    cleanup(&wal_path);
}

#[test]
fn reclaimed_wal_survives_immediate_process_kill() {
    let wal_path = temp_wal("checkpoint_reclaim");
    cleanup(&wal_path);

    {
        let mut tablet = WalBackedTablet::open(descriptor(), &wal_path).unwrap();
        commit_put(&mut tablet, b"k", b"v1");
        commit_put(&mut tablet, b"k", b"v2");
    }

    let ack = run_until_ack_then_kill(&wal_path, "checkpoint");
    assert_eq!(ack.trim(), "ACK CHECKPOINT_RECLAIMED 2");

    let mut tablet = WalBackedTablet::open(descriptor(), &wal_path).unwrap();
    assert_eq!(tablet.current_sequence(), 2);
    assert_eq!(tablet.read_at(b"k", 1).unwrap(), Some(&b"v1"[..]));

    commit_put(&mut tablet, b"k", b"v3");
    assert_eq!(tablet.current_sequence(), 3);

    cleanup(&wal_path);
}

#[test]
fn competing_process_cannot_write_until_checkpoint_owner_exits() {
    let wal_path = temp_wal("cross_process_lock");
    cleanup(&wal_path);

    let (mut child, ack) = spawn_until_ack(&wal_path, "hold-lock");
    let competing = WalBackedTablet::open(descriptor(), &wal_path);

    // Reap the parked child even if the assertion fails (RED test safety).
    child.0.kill().unwrap();
    assert!(!child.0.wait().unwrap().success());
    assert_eq!(ack.trim(), "ACK WRITER_LOCK 1");
    assert_eq!(
        competing.unwrap_err(),
        WalBackedError::Wal(WalError::WriterLocked),
        "a child retaining its writer lock must exclude this process"
    );

    let writer = WalBackedTablet::open(descriptor(), &wal_path).unwrap();
    assert_eq!(writer.current_sequence(), 1);
    assert_eq!(writer.read_latest(b"k"), Some(&b"from-child"[..]));
    drop(writer);
    cleanup(&wal_path);
}

#[test]
fn acknowledgement_timeout_reaps_the_child() {
    let wal_path = temp_wal("ack_timeout");
    let ack = ack_path(&wal_path);
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "crash_fixture_child", "--ignored", "--nocapture"])
        .env("NULANG_NUDB_CRASH_CHILD_WAL", &wal_path)
        .env("NULANG_NUDB_CRASH_CHILD_ACK", &ack)
        .env("NULANG_NUDB_CRASH_CHILD_ACTION", "never-ack")
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let result = wait_for_ack(&mut child, &ack, Duration::ZERO);
    let status = child.try_wait().unwrap();
    // Defensive cleanup even if the regression fails.
    if status.is_none() {
        let _ = child.kill();
        let _ = child.wait();
    }
    assert!(result.is_err());
    assert!(
        status.is_some(),
        "timed-out child must be killed and reaped"
    );
    cleanup(&wal_path);
}

#[cfg(unix)]
use nulang::database::{
    generation_registry::{GenerationRegistry, GenerationStatus, ReferenceKind},
    managed_storage::ManagedTablet,
    sst_manifest::ManifestStore,
    tablet::TransactionId,
};

#[cfg(unix)]
fn managed_mutation(value: &[u8]) -> Vec<TabletMutation> {
    vec![TabletMutation::Put {
        key: b"k".to_vec(),
        value: value.to_vec(),
    }]
}

#[cfg(unix)]
fn managed_id(n: u8) -> TransactionId {
    TransactionId::new([n; 16])
}

#[cfg(unix)]
fn managed_child(wal: &Path, action: &str) -> (ManagedTablet, String) {
    let source = wal.parent().unwrap();
    let old_dir = source.join("old");
    let registry_dir = source.join("registry");
    fs::create_dir(&old_dir).unwrap();
    fs::create_dir(&registry_dir).unwrap();
    let mut db = WalBackedTablet::open(descriptor(), wal).unwrap();
    let first = db
        .commit_idempotent(managed_id(1), managed_mutation(b"first"))
        .unwrap();
    assert_eq!(first.sequence(), 1);
    let mut old = ManifestStore::create(&old_dir, 81, 5).unwrap();
    db.flush_sst_shadow(&mut old, 1).unwrap();
    db.seal_sst_recovery(&mut old).unwrap();
    db.reclaim_to_sealed_sst(&old).unwrap();
    let registry = GenerationRegistry::create(&registry_dir, 81, 5).unwrap();
    let mut managed = ManagedTablet::adopt(db, registry, old).unwrap();
    managed
        .register_reference(ReferenceKind::Reader, [9; 16])
        .unwrap();
    let second = managed
        .commit_idempotent(managed_id(2), managed_mutation(b"tail"))
        .unwrap();
    assert_eq!(second.sequence(), 2);
    if action == "managed-enroll" {
        return (managed, "ACK MANAGED_ENROLLED 2".to_string());
    }
    let next_dir = source.join("next");
    fs::create_dir(&next_dir).unwrap();
    managed
        .rotate(ManifestStore::create(&next_dir, 81, 5).unwrap(), 1)
        .unwrap();
    managed
        .backup([8; 16], source.parent().unwrap().join("backup"))
        .unwrap();
    let third = managed
        .commit_idempotent(managed_id(3), managed_mutation(b"after-backup"))
        .unwrap();
    assert_eq!(third.sequence(), 3);
    (managed, "ACK MANAGED_ROTATED_BACKUP 3".to_string())
}

#[cfg(unix)]
struct ManagedCrashDir(PathBuf);
#[cfg(unix)]
impl ManagedCrashDir {
    fn new() -> Self {
        let root = temp_wal("managed-root").with_extension("dir");
        fs::create_dir(&root).unwrap();
        fs::create_dir(root.join("source")).unwrap();
        Self(root)
    }
}
#[cfg(unix)]
impl Drop for ManagedCrashDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[cfg(unix)]
fn kill_managed_after_ack(wal: &Path, action: &str, expected: &str) {
    use std::os::unix::process::ExitStatusExt;
    let (mut child, ack) = spawn_until_ack(wal, action);
    assert_eq!(ack.trim(), expected);
    let registry = wal.parent().unwrap().join("registry");
    assert_eq!(
        GenerationRegistry::open(&registry, 81, 5)
            .unwrap_err()
            .kind(),
        io::ErrorKind::WouldBlock
    );
    assert!(
        child.0.try_wait().unwrap().is_none(),
        "child must still own the locks"
    );
    child.0.kill().unwrap();
    assert_eq!(child.0.wait().unwrap().signal(), Some(9));
}

#[cfg(unix)]
#[test]
fn managed_enrollment_and_acknowledged_tail_survive_sigkill() {
    let fixture = ManagedCrashDir::new();
    let source = fixture.0.join("source");
    let wal = source.join("tablet.wal");
    kill_managed_after_ack(&wal, "managed-enroll", "ACK MANAGED_ENROLLED 2");
    let before = fs::read(&wal).unwrap();
    assert!(WalBackedTablet::open_from_sealed_sst(descriptor(), &wal, source.join("old")).is_err());
    assert_eq!(fs::read(&wal).unwrap(), before);
    let registry = GenerationRegistry::open(source.join("registry"), 81, 5).unwrap();
    let mut managed =
        ManagedTablet::open(descriptor(), &wal, source.join("old"), registry).unwrap();
    assert_eq!(managed.current_sequence(), 2);
    assert_eq!(managed.read_at(b"k", 1).unwrap(), Some(b"first".as_slice()));
    assert_eq!(managed.read_at(b"k", 2).unwrap(), Some(b"tail".as_slice()));
    assert_eq!(
        managed
            .commit_idempotent(managed_id(2), managed_mutation(b"tail"))
            .unwrap()
            .sequence(),
        2
    );
    assert!(managed
        .commit_idempotent(managed_id(2), managed_mutation(b"wrong"))
        .is_err());
    assert_eq!(fs::read(&wal).unwrap(), before);
    assert_eq!(
        managed
            .commit_idempotent(managed_id(3), managed_mutation(b"new"))
            .unwrap()
            .sequence(),
        3
    );
}

#[cfg(unix)]
#[test]
fn managed_rotation_and_backup_survive_sigkill_and_restore_without_source() {
    let fixture = ManagedCrashDir::new();
    let source = fixture.0.join("source");
    let wal = source.join("tablet.wal");
    kill_managed_after_ack(
        &wal,
        "managed-rotate-backup",
        "ACK MANAGED_ROTATED_BACKUP 3",
    );
    let bytes = fs::read(&wal).unwrap();
    assert!(
        WalBackedTablet::open_from_sealed_sst(descriptor(), &wal, source.join("next")).is_err()
    );
    let registry = GenerationRegistry::open(source.join("registry"), 81, 5).unwrap();
    assert!(ManagedTablet::open(descriptor(), &wal, source.join("old"), registry).is_err());
    assert_eq!(fs::read(&wal).unwrap(), bytes);
    let registry = GenerationRegistry::open(source.join("registry"), 81, 5).unwrap();
    let mut managed =
        ManagedTablet::open(descriptor(), &wal, source.join("next"), registry).unwrap();
    assert_eq!(managed.current_sequence(), 3);
    assert_eq!(managed.oldest_retained_snapshot(), 1);
    assert_eq!(
        managed.read_at(b"k", 3).unwrap(),
        Some(b"after-backup".as_slice())
    );
    let old = ManifestStore::open(source.join("old"), 81, 5).unwrap();
    assert_eq!(
        managed.generation_status(&old).unwrap(),
        GenerationStatus::Referenced
    );
    assert_eq!(
        managed
            .commit_idempotent(managed_id(3), managed_mutation(b"after-backup"))
            .unwrap()
            .sequence(),
        3
    );
    drop(old);
    drop(managed);
    fs::remove_dir_all(&source).unwrap(); // only the disposable fixture source
    assert!(!source.exists());
    let mut restored = WalBackedTablet::restore_sealed_backup(
        descriptor(),
        fixture.0.join("backup"),
        fixture.0.join("restored"),
    )
    .unwrap();
    assert_eq!(restored.current_sequence(), 2);
    assert_eq!(restored.oldest_retained_snapshot(), 1);
    assert_eq!(
        restored.read_at(b"k", 1).unwrap(),
        Some(b"first".as_slice())
    );
    assert_eq!(restored.read_latest(b"k"), Some(b"tail".as_slice()));
    assert_eq!(
        restored
            .commit_idempotent(managed_id(2), managed_mutation(b"tail"))
            .unwrap()
            .sequence(),
        2
    );
    assert!(restored
        .commit_idempotent(managed_id(2), managed_mutation(b"wrong"))
        .is_err());
    assert_eq!(
        restored
            .commit_idempotent(managed_id(3), managed_mutation(b"restored"))
            .unwrap()
            .sequence(),
        3
    );
}
