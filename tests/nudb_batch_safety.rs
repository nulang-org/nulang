//! Regressions for exclusive binary WAL ownership and bounded canonical batches.
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use nulang::database::tablet::{KeyRange, TabletDescriptor, TabletId, TabletMutation, TabletWrite};
use nulang::database::wal::FileWal;
use nulang::database::wal_batch::BinaryBatchWal;

static NEXT: AtomicU64 = AtomicU64::new(1);

fn path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "nudb_batch_safety_{name}_{}_{}.wal",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

fn descriptor() -> TabletDescriptor {
    TabletDescriptor::new(
        TabletId::new(123).unwrap(),
        KeyRange::new(Vec::new(), None).unwrap(),
        1,
    )
    .unwrap()
}

fn write(previous: u64, value: Vec<u8>) -> TabletWrite {
    TabletWrite::prepare(
        &descriptor(),
        1,
        previous,
        previous,
        vec![TabletMutation::Put {
            key: b"k".to_vec(),
            value,
        }],
    )
    .unwrap()
}

#[test]
fn binary_wal_fences_second_writer_until_the_first_releases_ownership() {
    let path = path("fence");
    let first = BinaryBatchWal::open(&path).unwrap();

    // Before the fix both handles are admitted, enabling conflicting tails.
    assert!(BinaryBatchWal::open(&path).is_err());
    assert_eq!(first.last_sequence(), 0);

    drop(first);
    let mut second = BinaryBatchWal::open(&path).unwrap();
    second
        .append_batch(&[write(0, b"durable".to_vec())])
        .unwrap();
    drop(second);

    let recovered = BinaryBatchWal::open(&path).unwrap();
    assert_eq!(recovered.last_sequence(), 1);
    drop(recovered);
    let _ = fs::remove_file(&path);
}

#[cfg(unix)]
#[test]
fn binary_wal_rejects_symlink_and_hardlink_file_aliases() {
    use std::os::unix::fs::symlink;
    let path = path("aliases");
    let alias = path.with_extension("alias");
    let lock = PathBuf::from(format!("{}.lock", path.display()));
    let original = BinaryBatchWal::open(&path).unwrap();

    symlink(&path, &alias).unwrap();
    assert!(BinaryBatchWal::open(&alias).is_err());
    fs::remove_file(&alias).unwrap();

    // A hardlink to the same WAL inode must not receive an independent lock.
    fs::hard_link(&path, &alias).unwrap();
    assert!(BinaryBatchWal::open(&alias).is_err());
    drop(original);
    assert!(BinaryBatchWal::open(&alias).is_err());
    fs::remove_file(&alias).unwrap();

    // A symlink at the lockfile path must also be rejected before mutation.
    let _ = fs::remove_file(&lock);
    symlink(&path, &lock).unwrap();
    assert!(BinaryBatchWal::open(&path).is_err());
    fs::remove_file(&lock).unwrap();
    let _ = fs::remove_file(&path);
}

#[test]
fn canonical_group_commit_rejects_excessive_write_count_before_disk_mutation() {
    let path = path("count");
    let mut wal = FileWal::open(&path).unwrap();
    let writes: Vec<_> = (0..4097).map(|i| write(i, b"v".to_vec())).collect();

    assert!(wal.append_batch(&writes).is_err());
    assert_eq!(wal.last_sequence(), 0);
    assert!(wal.records().is_empty());
    drop(wal);

    let recovered = FileWal::open(&path).unwrap();
    assert_eq!(recovered.last_sequence(), 0);
    drop(recovered);
    let _ = fs::remove_file(&path);
}

#[test]
fn canonical_group_commit_rejects_aggregate_byte_overflow_and_remains_writable() {
    let path = path("bytes");
    let mut wal = FileWal::open(&path).unwrap();
    let huge_value = vec![b'x'; 34 * 1024 * 1024];
    let writes = [write(0, huge_value.clone()), write(1, huge_value)];
    assert!(wal.append_batch(&writes).is_err());
    assert_eq!(wal.last_sequence(), 0);
    assert!(wal.records().is_empty());

    // Preflight rejection must not poison this handle or publish any prefix.
    wal.append_batch(&[write(0, b"accepted".to_vec())]).unwrap();
    drop(wal);
    let recovered = FileWal::open(&path).unwrap();
    assert_eq!(recovered.last_sequence(), 1);
    assert_eq!(recovered.records().len(), 1);
    drop(recovered);
    let _ = fs::remove_file(&path);
}

#[test]
fn binary_wal_child_lock_fixture() {
    let Ok(path) = std::env::var("NUDB_BINARY_CHILD_WAL") else {
        return;
    };
    assert!(BinaryBatchWal::open(&path).is_err());
}

#[test]
fn binary_wal_excludes_a_competing_process() {
    let path = path("process");
    let writer = BinaryBatchWal::open(&path).unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg("binary_wal_child_lock_fixture")
        .arg("--nocapture")
        .env("NUDB_BINARY_CHILD_WAL", &path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "independent process was able to acquire a second writer: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    drop(writer);
    let reopened = BinaryBatchWal::open(&path).unwrap();
    assert_eq!(reopened.last_sequence(), 0);
    drop(reopened);
    let _ = fs::remove_file(&path);
}
