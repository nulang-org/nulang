//! Recovery and single-writer invariants of the NuDB single-node tablet.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use nulang::database::store::WalBackedTablet;
use nulang::database::tablet::{KeyRange, TabletDescriptor, TabletId, TabletMutation};
use nulang::database::wal::FileWal;

static NEXT_PATH: AtomicU64 = AtomicU64::new(1);

fn wal_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "nudb_single_node_{name}_{}_{}.wal",
        std::process::id(),
        NEXT_PATH.fetch_add(1, Ordering::Relaxed)
    ))
}

fn descriptor() -> TabletDescriptor {
    TabletDescriptor::new(
        TabletId::new(337).unwrap(),
        KeyRange::new(b"a".to_vec(), Some(b"z".to_vec())).unwrap(),
        1,
    )
    .unwrap()
}

fn put(key: &[u8], value: &[u8]) -> TabletMutation {
    TabletMutation::Put {
        key: key.to_vec(),
        value: value.to_vec(),
    }
}

fn cleanup(path: &Path) {
    let _ = fs::remove_file(path);
    let _ = fs::remove_file(path.with_extension("checkpoint"));
    let mut lock = path.as_os_str().to_os_string();
    lock.push(".lock");
    let _ = fs::remove_file(PathBuf::from(lock));
}

#[test]
fn duplicate_key_in_one_commit_survives_checkpoint_and_restart() {
    let path = wal_path("duplicate");
    cleanup(&path);

    let mut tablet = WalBackedTablet::open(descriptor(), &path).unwrap();
    let write = tablet
        .prepare_write(
            1,
            0,
            vec![
                put(b"k", b"first"),
                TabletMutation::Delete { key: b"k".to_vec() },
                put(b"k", b"last"),
            ],
        )
        .unwrap();
    assert_eq!(tablet.commit(write).unwrap(), 1);
    assert_eq!(tablet.read_at(b"k", 1).unwrap(), Some(&b"last"[..]));

    tablet.checkpoint().unwrap();
    drop(tablet);

    let reopened = WalBackedTablet::open(descriptor(), &path).unwrap();
    assert_eq!(reopened.current_sequence(), 1);
    assert_eq!(reopened.read_at(b"k", 1).unwrap(), Some(&b"last"[..]));
    drop(reopened);
    cleanup(&path);
}

#[test]
fn second_writer_is_excluded_before_mutating_the_wal() {
    let path = wal_path("exclusive");
    cleanup(&path);

    let mut first = WalBackedTablet::open(descriptor(), &path).unwrap();
    let initial_size = fs::metadata(&path).unwrap().len();
    assert!(
        WalBackedTablet::open(descriptor(), &path).is_err(),
        "a second handle must not be admitted with a stale cached WAL tail"
    );
    assert_eq!(fs::metadata(&path).unwrap().len(), initial_size);

    let write = first.prepare_write(1, 0, vec![put(b"k", b"v1")]).unwrap();
    first.commit(write).unwrap();
    first.checkpoint().unwrap();
    assert!(
        FileWal::open(&path).is_err(),
        "checkpoint replacement must retain writer ownership"
    );
    drop(first);

    let reopened = WalBackedTablet::open(descriptor(), &path).unwrap();
    assert_eq!(reopened.read_latest(b"k"), Some(&b"v1"[..]));
    drop(reopened);
    cleanup(&path);
}

#[test]
fn child_writer_holds_lock_when_invoked_as_fixture() {
    use std::io::Write;

    let Some(path) = std::env::var_os("NUDB_LOCK_CHILD_PATH") else {
        return;
    };
    let ready = std::env::var_os("NUDB_LOCK_CHILD_READY").unwrap();
    let _wal = FileWal::open(&path).unwrap();
    let mut file = fs::File::create(ready).unwrap();
    file.write_all(b"locked").unwrap();
    file.sync_all().unwrap();
    loop {
        std::thread::park();
    }
}

#[test]
fn competing_process_is_fenced_and_owner_kill_releases_lock() {
    use std::process::{Command, Stdio};
    use std::thread;
    use std::time::Duration;

    let path = wal_path("cross_process");
    cleanup(&path);
    let ready = path.with_extension("ready");
    let _ = fs::remove_file(&ready);
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "child_writer_holds_lock_when_invoked_as_fixture",
            "--nocapture",
        ])
        .env("NUDB_LOCK_CHILD_PATH", &path)
        .env("NUDB_LOCK_CHILD_READY", &ready)
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();

    let mut acquired = false;
    for _ in 0..2000 {
        if fs::read(&ready).is_ok_and(|bytes| bytes == b"locked") {
            acquired = true;
            break;
        }
        if let Some(status) = child.try_wait().unwrap() {
            panic!("writer lock fixture exited before acquisition: {status}");
        }
        thread::sleep(Duration::from_millis(5));
    }
    if !acquired {
        let _ = child.kill();
        let _ = child.wait();
        panic!("writer lock fixture did not acquire lock");
    }

    let blocked = FileWal::open(&path).is_err();
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(blocked, "independent process must not gain writable access");

    let reopened = FileWal::open(&path).unwrap();
    assert_eq!(reopened.last_sequence(), 0);
    drop(reopened);
    fs::remove_file(ready).unwrap();
    cleanup(&path);
}

#[cfg(unix)]
#[test]
fn symlink_hardlink_and_relative_aliases_cannot_bypass_writer_lock() {
    use std::os::unix::fs::symlink;

    let path = wal_path("aliases");
    cleanup(&path);
    let symlink_path = path.with_extension("symlink");
    let hardlink_path = path.with_extension("hardlink");
    let _ = fs::remove_file(&symlink_path);
    let _ = fs::remove_file(&hardlink_path);

    let owner = FileWal::open(&path).unwrap();
    let relative_alias = path
        .parent()
        .unwrap()
        .join(".")
        .join(path.file_name().unwrap());
    assert!(FileWal::open(&relative_alias).is_err());

    symlink(&path, &symlink_path).unwrap();
    fs::hard_link(&path, &hardlink_path).unwrap();
    assert!(FileWal::open(&symlink_path).is_err());
    assert!(FileWal::open(&hardlink_path).is_err());

    drop(owner);
    fs::remove_file(&symlink_path).unwrap();
    fs::remove_file(&hardlink_path).unwrap();

    let reopened = FileWal::open(&path).unwrap();
    assert_eq!(reopened.last_sequence(), 0);
    drop(reopened);
    cleanup(&path);
}
