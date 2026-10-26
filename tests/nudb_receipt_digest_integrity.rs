//! A WAL frame can have a valid outer checksum and still carry a forged
//! transaction identity. Receipt replay must reject the inner digest mismatch.
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use nulang::database::store::{WalBackedError, WalBackedTablet};
use nulang::database::tablet::{
    KeyRange, TabletDescriptor, TabletId, TabletMutation, TransactionId,
};
use nulang::database::wal::WalError;

static NEXT: AtomicU64 = AtomicU64::new(1);
const WAL_HEADER_LEN: usize = 8 + 8 + 8 + 8 + 32;
const FRAME_HEADER_LEN: usize = 4 + 2 + 4 + 32;

fn descriptor() -> TabletDescriptor {
    TabletDescriptor::new(
        TabletId::new(811).unwrap(),
        KeyRange::new(b"a".to_vec(), Some(b"z".to_vec())).unwrap(),
        9,
    )
    .unwrap()
}

#[test]
fn mismatched_transaction_digest_is_rejected_even_with_valid_frame_checksum() {
    let path = std::env::temp_dir().join(format!(
        "nudb_receipt_forged_digest_{}_{}.wal",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let id = TransactionId::new([77; 16]);
    {
        let mut tablet = WalBackedTablet::open(descriptor(), &path).unwrap();
        tablet
            .commit_idempotent(
                id,
                vec![TabletMutation::Put {
                    key: b"k".to_vec(),
                    value: b"unchanged".to_vec(),
                }],
            )
            .unwrap();
    }

    let bytes = fs::read(&path).unwrap();
    assert_eq!(&bytes[..8], b"NUDBWAL3");
    let pos = WAL_HEADER_LEN;
    let len = u32::from_le_bytes(bytes[pos + 6..pos + 10].try_into().unwrap()) as usize;
    let payload_start = pos + FRAME_HEADER_LEN;
    let mut payload: serde_json::Value =
        serde_json::from_slice(&bytes[payload_start..payload_start + len]).unwrap();
    let digest = payload["identity"]["digest"].as_array_mut().unwrap();
    let prior = digest[0].as_u64().unwrap();
    digest[0] = serde_json::json!((prior + 1) % 256);
    let corrupt_payload = serde_json::to_vec(&payload).unwrap();

    // Recompute both checksums, so the outer frame is valid and only the
    // *semantic* transaction-digest consistency check can catch this.
    let mut frame_header = bytes[pos..pos + FRAME_HEADER_LEN].to_vec();
    frame_header[6..10].copy_from_slice(&(corrupt_payload.len() as u32).to_le_bytes());
    let checksum = *blake3::hash(&frame_header[..10]).as_bytes();
    frame_header[10..].copy_from_slice(&checksum);

    let mut altered = bytes[..pos].to_vec();
    altered.extend_from_slice(&frame_header);
    altered.extend_from_slice(&corrupt_payload);
    altered.extend_from_slice(blake3::hash(&corrupt_payload).as_bytes());
    fs::write(&path, altered).unwrap();

    let err = WalBackedTablet::open(descriptor(), &path).unwrap_err();
    assert!(
        matches!(
            err,
            WalBackedError::Wal(WalError::InvalidRecord { reason, .. })
                if reason.contains("transaction digest")
        ),
        "replay must reject inconsistent receipt digests even when WAL frame checksums match"
    );

    let _ = fs::remove_file(&path);
    let mut sidecar = path.into_os_string();
    sidecar.push(".lock");
    let _ = fs::remove_file(PathBuf::from(sidecar));
}
