//! Regression coverage for duplicate-key commits and persisted MVCC retention.
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use nulang::database::store::WalBackedTablet;
use nulang::database::tablet::{KeyRange, TabletDescriptor, TabletError, TabletId, TabletMutation};

static NEXT_TEST: AtomicU64 = AtomicU64::new(1);

fn temp_wal(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "nulang_nudb_mvcc_{name}_{}_{}.wal",
        std::process::id(),
        NEXT_TEST.fetch_add(1, Ordering::Relaxed)
    ))
}

fn descriptor() -> TabletDescriptor {
    TabletDescriptor::new(
        TabletId::new(81).unwrap(),
        KeyRange::new(b"a".to_vec(), Some(b"z".to_vec())).unwrap(),
        2,
    )
    .unwrap()
}

fn put(key: &[u8], value: &[u8]) -> TabletMutation {
    TabletMutation::Put {
        key: key.to_vec(),
        value: value.to_vec(),
    }
}

fn delete(key: &[u8]) -> TabletMutation {
    TabletMutation::Delete { key: key.to_vec() }
}

fn commit(tablet: &mut WalBackedTablet, mutations: Vec<TabletMutation>) {
    let previous = tablet.current_sequence();
    let write = tablet.prepare_write(2, previous, mutations).unwrap();
    tablet.commit(write).unwrap();
}

fn cleanup(path: &Path) {
    let _ = fs::remove_file(path);
    let _ = fs::remove_file(path.with_extension("checkpoint"));
    let mut lock = path.as_os_str().to_os_string();
    lock.push(".lock");
    let _ = fs::remove_file(PathBuf::from(lock));
}

#[test]
fn retention_keeps_snapshot_floor_and_rejects_expired_snapshots_after_restart() {
    let path = temp_wal("retention");
    cleanup(&path);

    {
        let mut tablet = WalBackedTablet::open(descriptor(), &path).unwrap();
        commit(&mut tablet, vec![put(b"k", b"v1"), put(b"deleted", b"old")]);
        commit(&mut tablet, vec![put(b"k", b"v2"), delete(b"deleted")]);
        commit(&mut tablet, vec![put(b"k", b"v3")]);

        tablet.reclaim_history_before(2).unwrap();
        assert_eq!(tablet.oldest_retained_snapshot(), 2);
        assert_eq!(tablet.read_at(b"k", 2).unwrap(), Some(&b"v2"[..]));
        assert_eq!(tablet.read_at(b"k", 3).unwrap(), Some(&b"v3"[..]));
        assert_eq!(tablet.read_at(b"deleted", 2).unwrap(), None);
        assert!(matches!(
            tablet.read_at(b"k", 1),
            Err(TabletError::SnapshotExpired {
                requested: 1,
                oldest_retained: 2
            })
        ));
    }

    {
        let mut tablet = WalBackedTablet::open(descriptor(), &path).unwrap();
        assert_eq!(tablet.oldest_retained_snapshot(), 2);
        assert_eq!(tablet.read_at(b"k", 2).unwrap(), Some(&b"v2"[..]));
        assert!(matches!(
            tablet.read_at(b"k", 1),
            Err(TabletError::SnapshotExpired {
                requested: 1,
                oldest_retained: 2
            })
        ));
        commit(&mut tablet, vec![put(b"k", b"v4")]);
        tablet.checkpoint().unwrap();
    }

    let reopened = WalBackedTablet::open(descriptor(), &path).unwrap();
    assert_eq!(reopened.oldest_retained_snapshot(), 2);
    assert_eq!(reopened.read_at(b"k", 4).unwrap(), Some(&b"v4"[..]));
    assert!(matches!(
        reopened.read_at(b"k", 0),
        Err(TabletError::SnapshotExpired {
            requested: 0,
            oldest_retained: 2
        })
    ));
    drop(reopened);
    cleanup(&path);
}

#[test]
fn retention_rejects_future_watermark_without_changing_state() {
    let path = temp_wal("future_floor");
    cleanup(&path);

    let mut tablet = WalBackedTablet::open(descriptor(), &path).unwrap();
    commit(&mut tablet, vec![put(b"k", b"v1")]);
    assert!(tablet.reclaim_history_before(2).is_err());
    assert_eq!(tablet.oldest_retained_snapshot(), 0);
    assert_eq!(tablet.read_at(b"k", 0).unwrap(), None);
    assert_eq!(tablet.read_at(b"k", 1).unwrap(), Some(&b"v1"[..]));

    drop(tablet);
    cleanup(&path);
}

#[test]
fn retention_floor_cannot_move_backward() {
    let path = temp_wal("monotonic");
    cleanup(&path);

    let mut tablet = WalBackedTablet::open(descriptor(), &path).unwrap();
    commit(&mut tablet, vec![put(b"k", b"v1")]);
    commit(&mut tablet, vec![put(b"k", b"v2")]);
    tablet.reclaim_history_before(2).unwrap();
    assert!(tablet.reclaim_history_before(1).is_err());
    assert_eq!(tablet.oldest_retained_snapshot(), 2);
    drop(tablet);
    cleanup(&path);
}

#[test]
fn independent_pins_protect_history_until_last_guard_drops() {
    let path = temp_wal("pins");
    let mut tablet = WalBackedTablet::open(descriptor(), &path).unwrap();
    commit(&mut tablet, vec![put(b"k", b"v1")]);
    let first = tablet.pin_snapshot(1).unwrap();
    let second = tablet.pin_snapshot(1).unwrap();
    commit(&mut tablet, vec![put(b"k", b"v2")]);
    assert_eq!(tablet.safe_retention_floor(), 1);
    assert!(tablet.reclaim_history_before(2).is_err());
    tablet.reclaim_history_before(1).unwrap();
    assert_eq!(tablet.read_at(b"k", 1).unwrap(), Some(&b"v1"[..]));
    drop(first);
    assert!(tablet.reclaim_history_before(2).is_err());
    drop(second);
    assert_eq!(tablet.safe_retention_floor(), 2);
    tablet.reclaim_history_before(2).unwrap();
    assert!(tablet.pin_snapshot(1).is_err());
    assert!(tablet.pin_snapshot(3).is_err());
    drop(tablet);
    let tablet = WalBackedTablet::open(descriptor(), &path).unwrap();
    assert_eq!(tablet.oldest_retained_snapshot(), 2);
    assert_eq!(tablet.read_latest(b"k"), Some(&b"v2"[..]));
    drop(tablet);
    cleanup(&path);
}

#[test]
fn checkpoint_format_prevents_silent_downgrade_and_rejects_invalid_floor() {
    let path = temp_wal("format");
    let mut tablet = WalBackedTablet::open(descriptor(), &path).unwrap();
    commit(&mut tablet, vec![put(b"k", b"v1")]);
    commit(&mut tablet, vec![put(b"k", b"v2")]);
    tablet.reclaim_history_before(2).unwrap();
    drop(tablet);
    let checkpoint = path.with_extension("checkpoint");
    let bytes = fs::read(&checkpoint).unwrap();
    let size = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
    let original: serde_json::Value = serde_json::from_slice(&bytes[12..12 + size]).unwrap();
    assert_eq!(original["version"], 2);
    assert_eq!(original["state"]["oldest_retained_snapshot"], 2);
    for variant in 0..3 {
        let mut invalid = original.clone();
        match variant {
            0 => {
                invalid["version"] = 1.into();
            }
            1 => {
                invalid["state"]
                    .as_object_mut()
                    .unwrap()
                    .remove("oldest_retained_snapshot");
            }
            _ => {
                invalid["state"]["oldest_retained_snapshot"] = 3.into();
            }
        }
        let payload = serde_json::to_vec(&invalid).unwrap();
        let mut encoded = bytes[..8].to_vec();
        encoded.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        encoded.extend_from_slice(&payload);
        encoded.extend_from_slice(blake3::hash(&payload).as_bytes());
        fs::write(&checkpoint, &encoded).unwrap();
        assert!(
            WalBackedTablet::open(descriptor(), &path).is_err(),
            "variant={variant}"
        );
        assert_eq!(fs::read(&checkpoint).unwrap(), encoded);
    }
    fs::write(&checkpoint, bytes).unwrap();
    let tablet = WalBackedTablet::open(descriptor(), &path).unwrap();
    assert_eq!(tablet.oldest_retained_snapshot(), 2);
    drop(tablet);
    cleanup(&path);
}

#[test]
fn pruning_keeps_anchor_and_reinsert_history_but_removes_obsolete_versions() {
    let path = temp_wal("anchor");
    let mut tablet = WalBackedTablet::open(descriptor(), &path).unwrap();
    commit(
        &mut tablet,
        vec![put(b"k", b"old"), put(b"gone", b"old"), put(b"r", b"old")],
    );
    commit(
        &mut tablet,
        vec![put(b"k", b"anchor"), delete(b"gone"), delete(b"r")],
    );
    commit(&mut tablet, vec![put(b"other", b"floor")]);
    commit(
        &mut tablet,
        vec![put(b"k", b"new"), put(b"r", b"reinserted")],
    );
    tablet.reclaim_history_before(3).unwrap();
    assert_eq!(tablet.read_at(b"k", 3).unwrap(), Some(&b"anchor"[..]));
    assert_eq!(tablet.read_at(b"k", 4).unwrap(), Some(&b"new"[..]));
    assert_eq!(tablet.read_at(b"r", 3).unwrap(), None);
    assert_eq!(tablet.read_at(b"r", 4).unwrap(), Some(&b"reinserted"[..]));
    assert_eq!(tablet.read_at(b"gone", 4).unwrap(), None);
    drop(tablet);
    let checkpoint = fs::read(path.with_extension("checkpoint")).unwrap();
    let len = u32::from_le_bytes(checkpoint[8..12].try_into().unwrap()) as usize;
    let disk: serde_json::Value = serde_json::from_slice(&checkpoint[12..12 + len]).unwrap();
    let rows = disk["state"]["rows"].as_array().unwrap();
    let row = |key: &[u8]| rows.iter().find(|r| r["key"] == serde_json::json!(key));
    assert!(row(b"gone").is_none());
    let seqs = |key: &[u8]| {
        row(key).unwrap()["versions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["sequence"].as_u64().unwrap())
            .collect::<Vec<_>>()
    };
    assert_eq!(seqs(b"k"), vec![2, 4]);
    assert_eq!(seqs(b"r"), vec![4]);
    let tablet = WalBackedTablet::open(descriptor(), &path).unwrap();
    assert_eq!(tablet.read_at(b"k", 3).unwrap(), Some(&b"anchor"[..]));
    assert_eq!(tablet.read_at(b"r", 3).unwrap(), None);
    drop(tablet);
    cleanup(&path);
}
