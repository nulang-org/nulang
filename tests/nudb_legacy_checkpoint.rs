//! Regression for v1 checkpoints written before duplicate-key MVCC coalescing.
//! A checksum-valid legacy checkpoint with repeated per-key commit sequences
//! must preserve the last mutation, without accepting arbitrary corruption.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use nulang::database::store::WalBackedTablet;
use nulang::database::tablet::{KeyRange, TabletDescriptor, TabletId, TabletMutation};
use serde_json::{json, Value};

static NEXT: AtomicU64 = AtomicU64::new(1);

fn test_wal(suffix: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "nudb_legacy_checkpoint_{suffix}_{}_{}.wal",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

fn descriptor() -> TabletDescriptor {
    TabletDescriptor::new(
        TabletId::new(987).unwrap(),
        KeyRange::new(b"a".to_vec(), Some(b"z".to_vec())).unwrap(),
        7,
    )
    .unwrap()
}

fn cleanup(path: &Path) {
    for suffix in [".lock", ".reclaim.tmp"] {
        let mut name = path.as_os_str().to_os_string();
        name.push(suffix);
        let _ = fs::remove_file(PathBuf::from(name));
    }
    let _ = fs::remove_file(path);
    let _ = fs::remove_file(path.with_extension("checkpoint"));
}

fn put(tablet: &mut WalBackedTablet, bytes: &[u8]) {
    let seq = tablet.current_sequence();
    let write = tablet
        .prepare_write(
            7,
            seq,
            vec![TabletMutation::Put {
                key: b"k".to_vec(),
                value: bytes.to_vec(),
            }],
        )
        .unwrap();
    assert_eq!(tablet.commit(write).unwrap(), seq + 1);
}

fn base_checkpoint(label: &str) -> PathBuf {
    let wal = test_wal(label);
    cleanup(&wal);
    let mut tablet = WalBackedTablet::open(descriptor(), &wal).unwrap();
    put(&mut tablet, b"v1");
    put(&mut tablet, b"v2");
    tablet.checkpoint().unwrap();
    drop(tablet);
    wal
}

fn edit_checkpoint(wal: &Path, change: impl FnOnce(&mut Value)) -> Vec<u8> {
    let path = wal.with_extension("checkpoint");
    let original = fs::read(&path).unwrap();
    assert_eq!(&original[..8], b"NUDBCKP1");
    let len = u32::from_le_bytes(original[8..12].try_into().unwrap()) as usize;
    assert_eq!(original.len(), 12 + len + 32);
    let payload = &original[12..12 + len];
    assert_eq!(&original[12 + len..], blake3::hash(payload).as_bytes());
    let mut disk: Value = serde_json::from_slice(payload).unwrap();
    change(&mut disk);
    let payload = serde_json::to_vec(&disk).unwrap();
    let mut edited = Vec::with_capacity(12 + payload.len() + 32);
    edited.extend_from_slice(b"NUDBCKP1");
    edited.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    edited.extend_from_slice(&payload);
    edited.extend_from_slice(blake3::hash(&payload).as_bytes());
    fs::write(path, &edited).unwrap();
    edited
}

#[test]
fn legacy_duplicate_sequences_recover_last_mutation_and_recheckpoint_canonically() {
    let wal = base_checkpoint("repair");
    edit_checkpoint(&wal, |disk| {
        let versions = disk["state"]["rows"][0]["versions"].as_array_mut().unwrap();
        assert_eq!(versions.len(), 2);
        // v1 encoded every mutation in a batch with the same commit sequence.
        versions.push(json!({"sequence": 2, "value": null}));
        versions.push(json!({"sequence": 2, "value": b"v2-last".to_vec()}));
    });

    let mut restored = WalBackedTablet::open(descriptor(), &wal).unwrap();
    assert_eq!(restored.current_sequence(), 2);
    assert_eq!(restored.read_at(b"k", 1).unwrap(), Some(&b"v1"[..]));
    assert_eq!(restored.read_at(b"k", 2).unwrap(), Some(&b"v2-last"[..]));
    put(&mut restored, b"v3");
    restored.checkpoint().unwrap();
    drop(restored);

    let reread = WalBackedTablet::open(descriptor(), &wal).unwrap();
    assert_eq!(reread.read_at(b"k", 1).unwrap(), Some(&b"v1"[..]));
    assert_eq!(reread.read_at(b"k", 2).unwrap(), Some(&b"v2-last"[..]));
    assert_eq!(reread.read_at(b"k", 3).unwrap(), Some(&b"v3"[..]));
    drop(reread);

    let current = fs::read(wal.with_extension("checkpoint")).unwrap();
    let len = u32::from_le_bytes(current[8..12].try_into().unwrap()) as usize;
    let current: Value = serde_json::from_slice(&current[12..12 + len]).unwrap();
    let versions = current["state"]["rows"][0]["versions"].as_array().unwrap();
    assert_eq!(
        versions.len(),
        3,
        "canonical checkpoint has one version per sequence"
    );
    assert_eq!(
        versions
            .iter()
            .map(|v| v["sequence"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    cleanup(&wal);
}

#[test]
fn legacy_repair_cannot_hide_decreasing_sequence_history() {
    for (label, mutate) in [
        ("reverse", vec![2, 1, 2]),
        ("zero", vec![1, 0, 2]),
        ("ahead", vec![1, 3, 2]),
    ] {
        let wal = base_checkpoint(label);
        let disk = edit_checkpoint(&wal, |disk| {
            let versions = disk["state"]["rows"][0]["versions"].as_array_mut().unwrap();
            versions.clear();
            for seq in mutate {
                versions.push(json!({"sequence":seq,"value":b"value".to_vec()}));
            }
        });
        let error = WalBackedTablet::open(descriptor(), &wal).unwrap_err();
        assert!(
            format!("{error}").contains("invalid MVCC version history"),
            "{label}: should fail closed, got {error}"
        );
        assert_eq!(fs::read(wal.with_extension("checkpoint")).unwrap(), disk);
        cleanup(&wal);
    }
}

#[test]
fn checksum_corruption_still_fails_closed_even_for_legacy_history() {
    let wal = base_checkpoint("checksum");
    edit_checkpoint(&wal, |disk| {
        let versions = disk["state"]["rows"][0]["versions"].as_array_mut().unwrap();
        versions.push(json!({"sequence":2,"value":b"last".to_vec()}));
    });
    let path = wal.with_extension("checkpoint");
    let mut bytes = fs::read(&path).unwrap();
    let payload_start = 12;
    bytes[payload_start + 5] ^= 0x01;
    fs::write(path, bytes).unwrap();
    let error = WalBackedTablet::open(descriptor(), &wal).unwrap_err();
    assert!(format!("{error}").contains("checksum mismatch"), "{error}");
    cleanup(&wal);
}
