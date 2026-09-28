use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use nulang::database::store::WalBackedTablet;
use nulang::database::tablet::{KeyRange, TabletDescriptor, TabletId, TabletMutation};

const CHILD_ENV: &str = "NULANG_NUDB_HARD_KILL_CHILD";
const WAL_ENV: &str = "NULANG_NUDB_HARD_KILL_WAL";
const ACK_PREFIX: &str = "NUDB_ACK:";
static NEXT_TEST: AtomicU64 = AtomicU64::new(1);

fn descriptor() -> TabletDescriptor {
    TabletDescriptor::new(
        TabletId::new(81).unwrap(),
        KeyRange::new(b"a".to_vec(), Some(b"z".to_vec())).unwrap(),
        1,
    )
    .unwrap()
}

fn temp_wal(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "nulang_nudb_hard_kill_{name}_{}_{}.wal",
        std::process::id(),
        NEXT_TEST.fetch_add(1, Ordering::Relaxed)
    ))
}

fn child_wal_path() -> PathBuf {
    PathBuf::from(std::env::var_os(WAL_ENV).expect("child WAL path"))
}

fn wait_for_ack(child: &mut std::process::Child) -> String {
    let stdout = child.stdout.take().expect("child stdout");
    let (sender, receiver) = mpsc::channel();

    thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            match line {
                Ok(line) if line.contains(ACK_PREFIX) => {
                    let _ = sender.send(line);
                    return;
                }
                Ok(_) => {}
                Err(_) => return,
            }
        }
    });

    receiver
        .recv_timeout(Duration::from_secs(30))
        .expect("child did not emit a durable acknowledgement")
}

fn remove_if_present(path: &Path) {
    let _ = fs::remove_file(path);
}

#[test]
fn nudb_hard_kill_child() {
    if std::env::var_os(CHILD_ENV).is_none() {
        return;
    }

    let wal_path = child_wal_path();
    let mut tablet = WalBackedTablet::open(descriptor(), &wal_path).unwrap();
    let previous = tablet.current_sequence();
    let write = tablet
        .prepare_write(
            1,
            previous,
            vec![TabletMutation::Put {
                key: b"k".to_vec(),
                value: b"acknowledged".to_vec(),
            }],
        )
        .unwrap();
    let sequence = tablet.commit(write).unwrap();

    println!("{ACK_PREFIX}{sequence}");
    std::io::stdout().flush().unwrap();

    // Remain alive so the parent can terminate us abruptly after observing
    // the acknowledgement. No graceful destructor/shutdown path is involved.
    loop {
        thread::sleep(Duration::from_secs(60));
    }
}

#[test]
fn acknowledged_commit_survives_abrupt_process_kill_and_restart() {
    let wal_path = temp_wal("ack");
    remove_if_present(&wal_path);

    let executable = std::env::current_exe().expect("current integration-test executable");
    let mut child = Command::new(executable)
        .arg("--exact")
        .arg("nudb_hard_kill_child")
        .arg("--nocapture")
        .arg("--test-threads=1")
        .env(CHILD_ENV, "1")
        .env(WAL_ENV, &wal_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn NuDB crash child");

    let ack = wait_for_ack(&mut child);
    assert!(
        ack.contains("NUDB_ACK:1"),
        "unexpected acknowledgement: {ack}"
    );

    child.kill().expect("abruptly kill NuDB child");
    child.wait().expect("reap NuDB child");

    let tablet = WalBackedTablet::open(descriptor(), &wal_path)
        .expect("restart from the same WAL after abrupt kill");
    assert_eq!(tablet.current_sequence(), 1);
    assert_eq!(tablet.read_at(b"k", 1).unwrap(), Some(&b"acknowledged"[..]));

    remove_if_present(&wal_path);
}
