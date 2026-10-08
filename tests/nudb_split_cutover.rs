//! Single-node NuDB split cutover: one recoverable catalog decision.
//! This is deliberately not a distributed consensus or cross-process lease.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use nulang::database::split::{SingleNodeSplitStore, SplitError};
use nulang::database::tablet::{KeyRange, TabletDescriptor, TabletId, TabletMutation};

static NEXT: AtomicU64 = AtomicU64::new(1);

fn temp_root(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "nulang_nudb_split_{name}_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

fn parent() -> TabletDescriptor {
    TabletDescriptor::new(
        TabletId::new(401).unwrap(),
        KeyRange::new(b"a".to_vec(), Some(b"z".to_vec())).unwrap(),
        7,
    )
    .unwrap()
}

fn put(key: &[u8], value: &[u8]) -> TabletMutation {
    TabletMutation::Put {
        key: key.to_vec(),
        value: value.to_vec(),
    }
}

#[test]
fn split_cutover_survives_restart_and_routes_independent_child_sequences() {
    let root = temp_root("cutover");
    {
        let mut store = SingleNodeSplitStore::open(&root, parent()).unwrap();
        store.commit(put(b"b", b"before")).unwrap();
        store.commit(put(b"m", b"middle")).unwrap();
        store.commit(put(b"y", b"right")).unwrap();

        let plan = parent()
            .plan_split(
                b"m",
                TabletId::new(402).unwrap(),
                TabletId::new(403).unwrap(),
                8,
            )
            .unwrap();
        store.split(&plan).unwrap();
        assert!(store.is_split());

        // Both children inherit the parent sequence (3) at the cutover.
        assert_eq!(store.read_at(b"b", 1).unwrap(), Some(b"before".to_vec()));
        assert_eq!(store.read_at(b"m", 2).unwrap(), Some(b"middle".to_vec()));
        assert_eq!(store.read_at(b"y", 3).unwrap(), Some(b"right".to_vec()));

        assert_eq!(store.commit(put(b"b", b"left-after")).unwrap(), 4);
        assert_eq!(store.commit(put(b"m", b"right-after")).unwrap(), 4);
        assert_eq!(store.commit(put(b"b", b"left-again")).unwrap(), 5);
        assert_eq!(
            store.read_at(b"b", 4).unwrap(),
            Some(b"left-after".to_vec())
        );
        assert_eq!(
            store.read_at(b"m", 4).unwrap(),
            Some(b"right-after".to_vec())
        );
    }

    let mut recovered = SingleNodeSplitStore::open(&root, parent()).unwrap();
    assert!(recovered.is_split());
    assert_eq!(
        recovered.read_latest(b"b").unwrap(),
        Some(b"left-again".to_vec())
    );
    assert_eq!(
        recovered.read_at(b"b", 1).unwrap(),
        Some(b"before".to_vec())
    );
    assert_eq!(
        recovered.read_latest(b"m").unwrap(),
        Some(b"right-after".to_vec())
    );
    assert_eq!(
        recovered.read_at(b"m", 2).unwrap(),
        Some(b"middle".to_vec())
    );
    assert_eq!(recovered.commit(put(b"y", b"after-recovery")).unwrap(), 5);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn invalid_cutover_plan_does_not_change_parent_state() {
    let root = temp_root("invalid");
    {
        let mut store = SingleNodeSplitStore::open(&root, parent()).unwrap();
        store.commit(put(b"b", b"still-here")).unwrap();

        let mut plan = parent()
            .plan_split(
                b"m",
                TabletId::new(402).unwrap(),
                TabletId::new(403).unwrap(),
                8,
            )
            .unwrap();
        plan.right = plan.left.clone();
        assert!(store.split(&plan).is_err());
        assert!(!store.is_split());
        assert_eq!(
            store.read_latest(b"b").unwrap(),
            Some(b"still-here".to_vec())
        );
    }
    let reopened = SingleNodeSplitStore::open(&root, parent()).unwrap();
    assert!(!reopened.is_split());
    assert_eq!(
        reopened.read_latest(b"b").unwrap(),
        Some(b"still-here".to_vec())
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn published_manifest_fails_closed_when_child_data_is_missing() {
    let root = temp_root("missing_child");
    {
        let mut store = SingleNodeSplitStore::open(&root, parent()).unwrap();
        store.commit(put(b"b", b"persist")).unwrap();
        store
            .split(
                &parent()
                    .plan_split(
                        b"m",
                        TabletId::new(402).unwrap(),
                        TabletId::new(403).unwrap(),
                        8,
                    )
                    .unwrap(),
            )
            .unwrap();
    }

    // The catalog must never silently route reads back to a stale parent.
    fs::remove_file(root.join("left.checkpoint")).unwrap();
    assert!(SingleNodeSplitStore::open(&root, parent()).is_err());
    let _ = fs::remove_dir_all(root);
}

#[test]
fn published_manifest_fails_closed_when_child_wal_is_missing_at_sequence_zero() {
    let root = temp_root("missing_zero_sequence_child");
    {
        let mut store = SingleNodeSplitStore::open(&root, parent()).unwrap();
        store
            .split(
                &parent()
                    .plan_split(
                        b"m",
                        TabletId::new(402).unwrap(),
                        TabletId::new(403).unwrap(),
                        8,
                    )
                    .unwrap(),
            )
            .unwrap();
    }

    fs::remove_file(root.join("right.wal")).unwrap();
    assert!(SingleNodeSplitStore::open(&root, parent()).is_err());
    let _ = fs::remove_dir_all(root);
}

#[test]
fn published_manifest_fails_closed_when_child_wal_is_truncated() {
    let root = temp_root("truncated_child");
    {
        let mut store = SingleNodeSplitStore::open(&root, parent()).unwrap();
        store
            .split(
                &parent()
                    .plan_split(
                        b"m",
                        TabletId::new(402).unwrap(),
                        TabletId::new(403).unwrap(),
                        8,
                    )
                    .unwrap(),
            )
            .unwrap();
    }
    // A fresh-child sequence of zero must not excuse a truncated WAL.
    fs::write(root.join("right.wal"), b"").unwrap();
    assert!(matches!(
        SingleNodeSplitStore::open(&root, parent()),
        Err(SplitError::InvalidManifest(_))
    ));
    let _ = fs::remove_dir_all(root);
}

#[test]
fn published_manifest_fails_closed_when_parent_wal_is_truncated() {
    let root = temp_root("truncated_parent");
    {
        let mut store = SingleNodeSplitStore::open(&root, parent()).unwrap();
        store.commit(put(b"b", b"persist")).unwrap();
    }
    fs::write(root.join("parent.wal"), b"").unwrap();
    assert!(matches!(
        SingleNodeSplitStore::open(&root, parent()),
        Err(SplitError::InvalidManifest(_))
    ));
    let _ = fs::remove_dir_all(root);
}

#[test]
fn corrupted_manifest_fails_closed_instead_of_reopening_parent() {
    let root = temp_root("corrupt_catalog");
    {
        let mut store = SingleNodeSplitStore::open(&root, parent()).unwrap();
        store.commit(put(b"b", b"persist")).unwrap();
    }
    let manifest = root.join("route.manifest");
    let mut bytes = fs::read(&manifest).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 0x80;
    fs::write(&manifest, bytes).unwrap();
    assert!(matches!(
        SingleNodeSplitStore::open(&root, parent()),
        Err(SplitError::InvalidManifest(_))
    ));
    let _ = fs::remove_dir_all(root);
}
