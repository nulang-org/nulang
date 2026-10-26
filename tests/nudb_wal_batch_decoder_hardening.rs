use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use nulang::database::wal_batch::{BatchWalError, BinaryBatchWal};

const FILE_MAGIC: &[u8; 8] = b"NUDBBW01";
const FRAME_MAGIC: &[u8; 4] = b"NBAT";
const FRAME_VERSION: u16 = 1;
const FRAME_PREFIX_BYTES: usize = 4 + 2 + 4 + 4;
const FRAME_HEADER_BYTES: usize = FRAME_PREFIX_BYTES + 32;
const MAX_MUTATIONS_PER_RECORD: u32 = 65_536;

static NEXT_TEST: AtomicU64 = AtomicU64::new(1);

fn temp_wal(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "nulang_nudb_batch_decoder_{name}_{}_{}.wal",
        std::process::id(),
        NEXT_TEST.fetch_add(1, Ordering::Relaxed)
    ))
}

fn record_prefix(mutation_count: u32) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&901_u64.to_le_bytes());
    payload.extend_from_slice(&7_u64.to_le_bytes());
    payload.extend_from_slice(&1_u64.to_le_bytes());
    payload.extend_from_slice(&0_u64.to_le_bytes());
    payload.extend_from_slice(&mutation_count.to_le_bytes());
    payload
}

fn write_checksum_valid_frame(path: &PathBuf, payload: &[u8]) {
    let mut header = [0_u8; FRAME_HEADER_BYTES];
    header[..4].copy_from_slice(FRAME_MAGIC);
    header[4..6].copy_from_slice(&FRAME_VERSION.to_le_bytes());
    header[6..10].copy_from_slice(&1_u32.to_le_bytes());
    header[10..14].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    let header_checksum = blake3::hash(&header[..FRAME_PREFIX_BYTES]);
    header[FRAME_PREFIX_BYTES..].copy_from_slice(header_checksum.as_bytes());

    let mut file = fs::File::create(path).unwrap();
    file.write_all(FILE_MAGIC).unwrap();
    file.write_all(&header).unwrap();
    file.write_all(payload).unwrap();
    file.write_all(blake3::hash(payload).as_bytes()).unwrap();
    file.sync_all().unwrap();
}

#[test]
fn checksum_valid_frame_rejects_excessive_mutation_count_before_allocation() {
    let path = temp_wal("mutation_count");
    let _ = fs::remove_file(&path);

    let payload = record_prefix(MAX_MUTATIONS_PER_RECORD + 1);
    write_checksum_valid_frame(&path, &payload);

    let error = BinaryBatchWal::open(&path).unwrap_err();
    match error {
        BatchWalError::InvalidRecord { reason, .. } => {
            assert!(
                reason.contains("mutation count") && reason.contains("limit"),
                "unexpected rejection reason: {reason}"
            );
        }
        other => panic!("expected invalid-record rejection, got {other:?}"),
    }

    let _ = fs::remove_file(&path);
}

#[test]
fn checksum_valid_frame_rejects_impossible_mutation_count_from_remaining_payload() {
    let path = temp_wal("remaining_mutations");
    let _ = fs::remove_file(&path);

    // Even below the explicit cap, 100 mutations cannot fit in a payload with
    // no mutation bytes after the fixed record prefix.
    let payload = record_prefix(100);
    write_checksum_valid_frame(&path, &payload);

    let error = BinaryBatchWal::open(&path).unwrap_err();
    match error {
        BatchWalError::InvalidRecord { reason, .. } => {
            assert!(
                reason.contains("mutation count") && reason.contains("remaining payload"),
                "unexpected rejection reason: {reason}"
            );
        }
        other => panic!("expected invalid-record rejection, got {other:?}"),
    }

    let _ = fs::remove_file(&path);
}

#[test]
fn checksum_valid_frame_rejects_byte_field_length_larger_than_remaining_payload() {
    let path = temp_wal("field_length");
    let _ = fs::remove_file(&path);

    let mut payload = record_prefix(1);
    payload.push(2); // delete mutation
    payload.extend_from_slice(&u32::MAX.to_le_bytes());
    write_checksum_valid_frame(&path, &payload);

    let error = BinaryBatchWal::open(&path).unwrap_err();
    match error {
        BatchWalError::InvalidRecord { reason, .. } => {
            assert!(
                reason.contains("byte field length") && reason.contains("remaining payload"),
                "unexpected rejection reason: {reason}"
            );
        }
        other => panic!("expected invalid-record rejection, got {other:?}"),
    }

    let _ = fs::remove_file(&path);
}

#[test]
fn checksum_valid_malformed_payload_corpus_never_panics() {
    let path = temp_wal("mutation_corpus");
    let _ = fs::remove_file(&path);

    let mut valid = record_prefix(1);
    valid.push(2); // delete mutation
    valid.extend_from_slice(&1_u32.to_le_bytes());
    valid.push(b'k');

    // Every strict truncation remains checksum-valid at the frame level but
    // must be rejected by the bounded payload decoder without panicking.
    for cut in 0..valid.len() {
        write_checksum_valid_frame(&path, &valid[..cut]);
        let result = std::panic::catch_unwind(|| BinaryBatchWal::open(&path));
        assert!(
            result.is_ok(),
            "decoder panicked for truncation at byte {cut}"
        );
        assert!(
            result.unwrap().is_err(),
            "truncated payload unexpectedly decoded at byte {cut}"
        );
    }

    // Flip one bit at every payload byte, recompute the frame checksum, and
    // require the parser to either reject the mutation or return a bounded
    // one-record result. This is a deterministic fuzz-style corpus that keeps
    // the repository's standard-test-only dependency policy intact.
    for index in 0..valid.len() {
        let mut mutated = valid.clone();
        mutated[index] ^= 0x80;
        write_checksum_valid_frame(&path, &mutated);
        let result = std::panic::catch_unwind(|| BinaryBatchWal::open(&path));
        assert!(
            result.is_ok(),
            "decoder panicked after mutating byte {index}"
        );
        if let Ok(wal) = result.unwrap() {
            assert!(wal.records().len() <= 1);
            assert!(wal.last_sequence() <= 1);
        }
    }

    let _ = fs::remove_file(&path);
}

#[test]
fn checksum_valid_frame_header_mutation_corpus_never_panics_or_overallocates() {
    let path = temp_wal("header_corpus");
    let _ = fs::remove_file(&path);

    let mut payload = record_prefix(1);
    payload.push(2);
    payload.extend_from_slice(&1_u32.to_le_bytes());
    payload.push(b'k');

    let mut valid_header = [0_u8; FRAME_HEADER_BYTES];
    valid_header[..4].copy_from_slice(FRAME_MAGIC);
    valid_header[4..6].copy_from_slice(&FRAME_VERSION.to_le_bytes());
    valid_header[6..10].copy_from_slice(&1_u32.to_le_bytes());
    valid_header[10..14].copy_from_slice(&(payload.len() as u32).to_le_bytes());

    for index in 0..FRAME_PREFIX_BYTES {
        let mut header = valid_header;
        header[index] ^= 0x80;
        let checksum = blake3::hash(&header[..FRAME_PREFIX_BYTES]);
        header[FRAME_PREFIX_BYTES..].copy_from_slice(checksum.as_bytes());

        let mut file = fs::File::create(&path).unwrap();
        file.write_all(FILE_MAGIC).unwrap();
        file.write_all(&header).unwrap();
        file.write_all(&payload).unwrap();
        file.write_all(blake3::hash(&payload).as_bytes()).unwrap();
        file.sync_all().unwrap();
        drop(file);

        let result = std::panic::catch_unwind(|| BinaryBatchWal::open(&path));
        assert!(
            result.is_ok(),
            "frame decoder panicked after mutating byte {index}"
        );
        if let Ok(wal) = result.unwrap() {
            assert!(wal.records().len() <= 1);
            assert!(wal.last_sequence() <= 1);
        }
    }

    let _ = fs::remove_file(&path);
}
