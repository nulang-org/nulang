use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use nulang::database::store::WalBackedTablet;
use nulang::database::tablet::{KeyRange, TabletDescriptor, TabletId, TabletMutation};
use nulang::database::wal::{FileWal, WalError};

static NEXT_TEST: AtomicU64 = AtomicU64::new(1);

fn wal_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "nudb_read_only_audit_{name}_{}_{}.wal",
        std::process::id(),
        NEXT_TEST.fetch_add(1, Ordering::Relaxed)
    ))
}

fn descriptor() -> TabletDescriptor {
    TabletDescriptor::new(
        TabletId::new(93).unwrap(),
        KeyRange::new(b"a".to_vec(), Some(b"z".to_vec())).unwrap(),
        3,
    )
    .unwrap()
}

fn fixture(path: &PathBuf) {
    let mut tablet = WalBackedTablet::open(descriptor(), path).unwrap();
    for (key, value) in [(b"k".as_slice(), b"one".as_slice()), (b"k", b"two")] {
        let sequence = tablet.current_sequence();
        let write = tablet
            .prepare_write(
                3,
                sequence,
                vec![TabletMutation::Put {
                    key: key.to_vec(),
                    value: value.to_vec(),
                }],
            )
            .unwrap();
        tablet.commit(write).unwrap();
    }
}

#[test]
fn audit_valid_wal_reports_metadata_without_creating_or_repairing_files() {
    let path = wal_path("valid");
    fixture(&path);
    let before = fs::read(&path).unwrap();
    let report = FileWal::verify_read_only(&path).unwrap();
    assert_eq!(report.base_sequence, 0);
    assert_eq!(report.last_sequence, 2);
    assert_eq!(report.record_count, 2);
    assert_eq!(report.tablet_id.unwrap().get(), 93);
    assert_eq!(report.last_ownership_epoch, Some(3));
    assert_eq!(report.bytes_checked, before.len() as u64);
    assert_eq!(fs::read(&path).unwrap(), before);
    fs::remove_file(&path).unwrap();
}

#[test]
fn partial_final_header_is_reported_without_truncating_crash_evidence() {
    let path = wal_path("partial_header");
    fixture(&path);
    OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"NRE")
        .unwrap();
    let before = fs::read(&path).unwrap();
    assert!(matches!(
        FileWal::verify_read_only(&path),
        Err(WalError::IncompleteFrame { .. })
    ));
    assert_eq!(fs::read(&path).unwrap(), before);
    fs::remove_file(&path).unwrap();
}

#[test]
fn incomplete_payload_and_checksum_are_not_repaired() {
    for (name, include_payload) in [("partial_payload", false), ("partial_checksum", true)] {
        let path = wal_path(name);
        fixture(&path);
        let source = fs::read(&path).unwrap();
        // Reuse one complete frame's *header* only. Stop before its payload
        // completes (or before its checksum completes), so validation must
        // report the torn suffix before any duplicate sequence could matter.
        const FILE_HEADER_BYTES: usize = 64;
        const FRAME_HEADER_BYTES: usize = 42;
        let payload_len = u32::from_le_bytes(source[70..74].try_into().unwrap()) as usize;
        let partial_bytes = if include_payload {
            FRAME_HEADER_BYTES + payload_len + 1
        } else {
            FRAME_HEADER_BYTES + payload_len - 1
        };
        let suffix = &source[FILE_HEADER_BYTES..FILE_HEADER_BYTES + partial_bytes];
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(suffix)
            .unwrap();
        let before = fs::read(&path).unwrap();
        assert!(
            matches!(
                FileWal::verify_read_only(&path),
                Err(WalError::IncompleteFrame { .. })
            ),
            "case: {name}"
        );
        assert_eq!(fs::read(&path).unwrap(), before, "case: {name}");
        fs::remove_file(&path).unwrap();
    }
}

#[test]
fn checksum_corruption_fails_closed_and_does_not_rewrite_wal() {
    let path = wal_path("bad_checksum");
    fixture(&path);
    let mut data = fs::read(&path).unwrap();
    *data.last_mut().unwrap() ^= 0x80;
    fs::write(&path, &data).unwrap();
    assert!(matches!(
        FileWal::verify_read_only(&path),
        Err(WalError::ChecksumMismatch { .. })
    ));
    assert_eq!(fs::read(&path).unwrap(), data);
    fs::remove_file(&path).unwrap();
}

#[test]
fn compacted_wal_with_no_frames_verifies_its_checkpoint_base() {
    let path = wal_path("compacted");
    fixture(&path);
    {
        let mut tablet = WalBackedTablet::open(descriptor(), &path).unwrap();
        tablet.checkpoint().unwrap();
    }
    let report = FileWal::verify_read_only(&path).unwrap();
    assert_eq!(
        (
            report.base_sequence,
            report.last_sequence,
            report.record_count
        ),
        (2, 2, 0)
    );
    fs::remove_file(path.with_extension("checkpoint")).unwrap();
    fs::remove_file(path).unwrap();
}

#[test]
fn missing_wal_must_not_be_created_by_audit() {
    let path = wal_path("missing");
    let error = FileWal::verify_read_only(&path).unwrap_err();
    assert!(matches!(
        error,
        WalError::Io {
            kind: std::io::ErrorKind::NotFound,
            ..
        }
    ));
    assert!(!path.exists());
}
