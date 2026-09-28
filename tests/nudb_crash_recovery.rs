use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use nulang::database::store::WalBackedTablet;
use nulang::database::tablet::{KeyRange, TabletDescriptor, TabletId, TabletMutation};

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

fn cleanup(wal_path: &Path) {
    let _ = fs::remove_file(wal_path);
    let _ = fs::remove_file(wal_path.with_extension("checkpoint"));
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

fn run_until_ack_then_kill(wal_path: &Path, action: &str) -> String {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "crash_fixture_child", "--ignored", "--nocapture"])
        .env("NULANG_NUDB_CRASH_CHILD_WAL", wal_path)
        .env("NULANG_NUDB_CRASH_CHILD_ACTION", action)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();

    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);
    let mut ack = String::new();
    for _ in 0..16 {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap() == 0 {
            break;
        }
        if line.starts_with("ACK ") {
            ack = line;
            break;
        }
    }

    // Always terminate the fixture before asserting on its output so a failed
    // acknowledgement cannot leak a parked child process into later tests.
    // std::process::Child::kill maps to abrupt process termination on the host
    // platform.
    child.kill().unwrap();
    let status = child.wait().unwrap();

    assert!(
        ack.starts_with("ACK "),
        "fixture must acknowledge only after its durability boundary, got {ack:?}"
    );
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

    match action.as_str() {
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
            println!("ACK COMMIT {committed}");
        }
        "publish-checkpoint" => {
            let tablet = WalBackedTablet::open(descriptor(), &wal_path).unwrap();
            let sequence = tablet.current_sequence();
            tablet.publish_checkpoint().unwrap();
            println!("ACK CHECKPOINT {sequence}");
        }
        "checkpoint" => {
            let mut tablet = WalBackedTablet::open(descriptor(), &wal_path).unwrap();
            let sequence = tablet.current_sequence();
            tablet.checkpoint().unwrap();
            println!("ACK CHECKPOINT_RECLAIMED {sequence}");
        }
        other => panic!("unknown crash fixture action: {other}"),
    }
    std::io::stdout().flush().unwrap();

    // A successful fixture never exits normally. The parent observes the
    // durability ACK, then kills this process immediately.
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
