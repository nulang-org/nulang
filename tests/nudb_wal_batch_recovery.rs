use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

use nulang::database::tablet::{
    KeyRange, MemoryTablet, TabletDescriptor, TabletId, TabletMutation,
};
use nulang::database::wal_batch::{BatchWalError, BinaryBatchWal};

static NEXT_TEST: AtomicU64 = AtomicU64::new(1);

fn temp_wal(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "nulang_nudb_binary_batch_recovery_{name}_{}_{}.wal",
        std::process::id(),
        NEXT_TEST.fetch_add(1, Ordering::Relaxed)
    ))
}

fn descriptor() -> TabletDescriptor {
    TabletDescriptor::new(
        TabletId::new(901).unwrap(),
        KeyRange::new(Vec::new(), None).unwrap(),
        7,
    )
    .unwrap()
}

fn build_writes(count: usize) -> Vec<nulang::database::tablet::TabletWrite> {
    let mut tablet = MemoryTablet::new(descriptor());
    let mut writes = Vec::with_capacity(count);
    for i in 0..count {
        let previous = tablet.current_sequence();
        let write = tablet
            .prepare_write(
                7,
                previous,
                vec![TabletMutation::Put {
                    key: format!("key-{i}").into_bytes(),
                    value: format!("value-{i}").into_bytes(),
                }],
            )
            .unwrap();
        tablet.commit(write.clone()).unwrap();
        writes.push(write);
    }
    writes
}

fn ack_path(wal: &Path) -> PathBuf {
    wal.with_extension("ack")
}

fn cleanup(wal: &Path) {
    let _ = fs::remove_file(wal);
    let _ = fs::remove_file(ack_path(wal));
}

#[test]
fn truncated_final_batch_is_discarded_as_a_crash_tail() {
    let path = temp_wal("truncated");
    cleanup(&path);

    let writes = build_writes(2);
    let mut wal = BinaryBatchWal::open(&path).unwrap();
    wal.append_batch(&writes[..1]).unwrap();
    let first_batch_end = fs::metadata(&path).unwrap().len();
    wal.append_batch(&writes[1..]).unwrap();
    drop(wal);

    let full_len = fs::metadata(&path).unwrap().len();
    assert!(full_len > first_batch_end + 8);
    fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(full_len - 8)
        .unwrap();

    let reopened = BinaryBatchWal::open(&path).unwrap();
    assert_eq!(reopened.last_sequence(), 1);
    assert_eq!(reopened.records().len(), 1);
    assert_eq!(fs::metadata(&path).unwrap().len(), first_batch_end);

    cleanup(&path);
}

#[test]
fn complete_payload_corruption_fails_closed() {
    let path = temp_wal("corrupt");
    cleanup(&path);

    let writes = build_writes(1);
    let mut wal = BinaryBatchWal::open(&path).unwrap();
    wal.append_batch(&writes).unwrap();
    drop(wal);

    // File header is 8 bytes; batch frame header is 14 bytes plus a 32-byte
    // BLAKE3 digest. Flip the first payload byte without changing framing.
    let payload_start = 8 + 14 + 32;
    let mut file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    file.seek(SeekFrom::Start(payload_start)).unwrap();
    let mut byte = [0_u8; 1];
    file.read_exact(&mut byte).unwrap();
    byte[0] ^= 0x40;
    file.seek(SeekFrom::Start(payload_start)).unwrap();
    file.write_all(&byte).unwrap();
    file.sync_all().unwrap();
    drop(file);

    assert!(matches!(
        BinaryBatchWal::open(&path).unwrap_err(),
        BatchWalError::PayloadChecksumMismatch { .. }
    ));

    cleanup(&path);
}

#[test]
#[ignore = "subprocess fixture; invoked explicitly by parent recovery test"]
fn binary_batch_crash_fixture_child() {
    let Some(wal_path) = std::env::var_os("NULANG_NUDB_BATCH_CHILD_WAL") else {
        return;
    };
    let ack_path = std::env::var_os("NULANG_NUDB_BATCH_CHILD_ACK")
        .expect("batch crash fixture ack path must be supplied");

    let writes = build_writes(2);
    let mut wal = BinaryBatchWal::open(&wal_path).unwrap();
    wal.append_batch(&writes).unwrap();
    assert_eq!(wal.last_sequence(), 2);

    let ack_path = PathBuf::from(ack_path);
    let mut temp = ack_path.as_os_str().to_os_string();
    temp.push(".tmp");
    let temp = PathBuf::from(temp);
    let mut file = fs::File::create(&temp).unwrap();
    file.write_all(b"ACK BATCH 2\n").unwrap();
    file.sync_data().unwrap();
    drop(file);
    fs::rename(&temp, &ack_path).unwrap();

    loop {
        thread::park();
    }
}

#[test]
fn acknowledged_binary_batch_survives_immediate_process_kill() {
    let path = temp_wal("hard_kill");
    cleanup(&path);
    let ack = ack_path(&path);

    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "binary_batch_crash_fixture_child",
            "--ignored",
            "--nocapture",
        ])
        .env("NULANG_NUDB_BATCH_CHILD_WAL", &path)
        .env("NULANG_NUDB_BATCH_CHILD_ACK", &ack)
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();

    let mut acknowledgement = None;
    for _ in 0..1_000 {
        if let Ok(value) = fs::read_to_string(&ack) {
            acknowledgement = Some(value);
            break;
        }
        if let Some(status) = child.try_wait().unwrap() {
            panic!("batch crash fixture exited before acknowledgement: {status}");
        }
        thread::sleep(Duration::from_millis(5));
    }

    child.kill().unwrap();
    let status = child.wait().unwrap();
    assert!(!status.success(), "fixture unexpectedly exited gracefully");
    assert_eq!(
        acknowledgement
            .expect("fixture did not publish a durability acknowledgement")
            .trim(),
        "ACK BATCH 2"
    );

    let reopened = BinaryBatchWal::open(&path).unwrap();
    assert_eq!(reopened.last_sequence(), 2);
    assert_eq!(reopened.records().len(), 2);
    assert_eq!(
        reopened.records()[1].mutations()[0],
        TabletMutation::Put {
            key: b"key-1".to_vec(),
            value: b"value-1".to_vec(),
        }
    );

    cleanup(&path);
}
