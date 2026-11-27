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
