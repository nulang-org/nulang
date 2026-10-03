use super::compaction::{compact_tablet_sstables_to_v2, CompactionError};
use super::manifest::{Manifest, ManifestEntry, SstableFormat, SstableIntegrity};
use super::sstable;
use super::sstable_v2::{write_sstable_v2, SstableV2};
use super::store::WalBackedTablet;
use super::tablet::{
    KeyRange, TabletDescriptor, TabletId, TabletMutation, TabletSnapshotRow, VersionedValue,
};
use super::wal::FileWal;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_TEST: AtomicU64 = AtomicU64::new(1);

fn temp_wal(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "nulang_nudb_compaction_{name}_{}_{}.wal",
        std::process::id(),
        NEXT_TEST.fetch_add(1, Ordering::Relaxed)
    ))
}

fn descriptor() -> TabletDescriptor {
    TabletDescriptor::new(
        TabletId::new(906).unwrap(),
        KeyRange::new(b"a".to_vec(), Some(b"z".to_vec())).unwrap(),
        1,
    )
    .unwrap()
}

fn cleanup(path: &Path) {
    let _ = fs::remove_file(path);
    let _ = fs::remove_file(path.with_extension("checkpoint"));
    let _ = fs::remove_file(path.with_extension("manifest"));
    let _ = fs::remove_dir_all(path.with_extension("sstables"));
}

fn commit_put(tablet: &mut WalBackedTablet, key: &[u8], value: &[u8]) {
    let sequence = tablet.current_sequence();
    let write = tablet
        .prepare_write(
            1,
            sequence,
            vec![TabletMutation::Put {
                key: key.to_vec(),
                value: value.to_vec(),
            }],
        )
        .unwrap();
    tablet.commit(write).unwrap();
}

fn commit_delete(tablet: &mut WalBackedTablet, key: &[u8]) {
    let sequence = tablet.current_sequence();
    let write = tablet
        .prepare_write(1, sequence, vec![TabletMutation::Delete { key: key.to_vec() }])
        .unwrap();
    tablet.commit(write).unwrap();
}

fn flush_current(tablet: &mut WalBackedTablet) {
    let bytes = tablet.mutable_memtable_bytes();
    assert!(bytes > 0);
    assert!(tablet.rotate_memtable_if_bytes_at_least(bytes));
    assert!(tablet.flush_oldest_immutable_to_sstable().unwrap());
}

fn install_v1_manifest(path: &Path, file_name: &str, rows: &[TabletSnapshotRow]) {
    let sstable_dir = path.with_extension("sstables");
    fs::create_dir_all(&sstable_dir).unwrap();
    let metadata = sstable::write_sstable(&sstable_dir.join(file_name), 906, 1, rows).unwrap();
    let mut manifest = Manifest::empty(906);
    manifest
        .register(ManifestEntry {
            file_name: metadata.file_name.clone(),
            tablet_id: metadata.tablet_id,
            ownership_epoch: metadata.ownership_epoch,
            min_sequence: metadata.min_sequence,
            max_sequence: metadata.max_sequence,
            row_count: metadata.row_count,
            min_key: metadata.min_key.clone(),
            max_key: metadata.max_key.clone(),
            format: SstableFormat::V1,
            integrity: SstableIntegrity::WholePayloadBlake3(metadata.checksum),
        })
        .unwrap();
    manifest.publish(&path.with_extension("manifest")).unwrap();
}

#[test]
fn compact_mixed_v1_v2_tables_preserves_full_mvcc_history_and_recovery() {
    let path = temp_wal("mixed");
    cleanup(&path);

    {
        let mut tablet = WalBackedTablet::open(descriptor(), &path).unwrap();
        commit_put(&mut tablet, b"k", b"v1");
    }
    install_v1_manifest(
        &path,
        "legacy-v1.sst",
        &[TabletSnapshotRow {
            key: b"k".to_vec(),
            versions: vec![VersionedValue {
                sequence: 1,
                value: Some(b"v1".to_vec()),
            }],
        }],
    );
    let mut wal = FileWal::open(&path).unwrap();
    wal.reclaim_through(1).unwrap();
    drop(wal);

    {
        let mut tablet = WalBackedTablet::open(descriptor(), &path).unwrap();
        commit_put(&mut tablet, b"k", b"v2");
        flush_current(&mut tablet);
        commit_delete(&mut tablet, b"k");
        flush_current(&mut tablet);
        assert_eq!(tablet.durable_sstable_count().unwrap(), 3);
    }

    assert!(compact_tablet_sstables_to_v2(&descriptor(), &path).unwrap());

    let manifest = Manifest::load_or_empty(&path.with_extension("manifest"), 906).unwrap();
    assert_eq!(manifest.entries().len(), 1);
    assert_eq!(manifest.entries()[0].format, SstableFormat::V2);
    assert!(manifest.entries()[0].file_name.contains("-compact-v2-"));
    assert_eq!(
        fs::read_dir(path.with_extension("sstables"))
            .unwrap()
            .count(),
        1
    );

    let mut wal = FileWal::open(&path).unwrap();
    wal.reclaim_through(3).unwrap();
    drop(wal);

    let reopened = WalBackedTablet::open(descriptor(), &path).unwrap();
    assert_eq!(reopened.current_sequence(), 3);
    assert_eq!(reopened.read_at(b"k", 1).unwrap(), Some(&b"v1"[..]));
    assert_eq!(reopened.read_at(b"k", 2).unwrap(), Some(&b"v2"[..]));
    assert_eq!(reopened.read_latest(b"k").unwrap(), None);

    cleanup(&path);
}

#[test]
fn compact_single_legacy_v1_table_rewrites_it_to_v2() {
    let path = temp_wal("single-v1");
    cleanup(&path);

    {
        let mut tablet = WalBackedTablet::open(descriptor(), &path).unwrap();
        commit_put(&mut tablet, b"k", b"legacy");
    }
    install_v1_manifest(
        &path,
        "legacy-only.sst",
        &[TabletSnapshotRow {
            key: b"k".to_vec(),
            versions: vec![VersionedValue {
                sequence: 1,
                value: Some(b"legacy".to_vec()),
            }],
        }],
    );
    let mut wal = FileWal::open(&path).unwrap();
    wal.reclaim_through(1).unwrap();
    drop(wal);

    assert!(compact_tablet_sstables_to_v2(&descriptor(), &path).unwrap());

    let manifest = Manifest::load_or_empty(&path.with_extension("manifest"), 906).unwrap();
    assert_eq!(manifest.entries().len(), 1);
    assert_eq!(manifest.entries()[0].format, SstableFormat::V2);
    assert!(matches!(
        manifest.entries()[0].integrity,
        SstableIntegrity::FooterBlake3(_)
    ));
    let compacted = SstableV2::open(
        &path
            .with_extension("sstables")
            .join(&manifest.entries()[0].file_name),
    )
    .unwrap();
    assert_eq!(
        compacted.snapshot_rows().unwrap()[0].versions[0]
            .value
            .as_deref(),
        Some(&b"legacy"[..])
    );
    assert!(!path
        .with_extension("sstables")
        .join("legacy-only.sst")
        .exists());

    cleanup(&path);
}

#[test]
fn compaction_conflict_fails_before_manifest_replacement() {
    let path = temp_wal("conflict");
    cleanup(&path);

    {
        let mut tablet = WalBackedTablet::open(descriptor(), &path).unwrap();
        commit_put(&mut tablet, b"k", b"wal-value");
    }
    let mut wal = FileWal::open(&path).unwrap();
    wal.reclaim_through(1).unwrap();
    drop(wal);

    let sstable_dir = path.with_extension("sstables");
    fs::create_dir_all(&sstable_dir).unwrap();
    let left_rows = vec![TabletSnapshotRow {
        key: b"k".to_vec(),
        versions: vec![VersionedValue {
            sequence: 1,
            value: Some(b"left".to_vec()),
        }],
    }];
    let right_rows = vec![TabletSnapshotRow {
        key: b"k".to_vec(),
        versions: vec![VersionedValue {
            sequence: 1,
            value: Some(b"right".to_vec()),
        }],
    }];
    let left =
        sstable::write_sstable(&sstable_dir.join("left-v1.sst"), 906, 1, &left_rows).unwrap();
    let right = write_sstable_v2(&sstable_dir.join("right-v2.sst"), 906, 1, &right_rows).unwrap();

    let mut manifest = Manifest::empty(906);
    manifest
        .register(ManifestEntry {
            file_name: left.file_name.clone(),
            tablet_id: left.tablet_id,
            ownership_epoch: left.ownership_epoch,
            min_sequence: left.min_sequence,
            max_sequence: left.max_sequence,
            row_count: left.row_count,
            min_key: left.min_key.clone(),
            max_key: left.max_key.clone(),
            format: SstableFormat::V1,
            integrity: SstableIntegrity::WholePayloadBlake3(left.checksum),
        })
        .unwrap();
    manifest
        .register(ManifestEntry {
            file_name: right.file_name.clone(),
            tablet_id: right.tablet_id,
            ownership_epoch: right.ownership_epoch,
            min_sequence: right.min_sequence,
            max_sequence: right.max_sequence,
            row_count: right.row_count,
            min_key: right.min_key.clone(),
            max_key: right.max_key.clone(),
            format: SstableFormat::V2,
            integrity: SstableIntegrity::FooterBlake3(right.footer_checksum),
        })
        .unwrap();
    manifest
        .publish(&path.with_extension("manifest"))
        .unwrap();

    assert_eq!(
        compact_tablet_sstables_to_v2(&descriptor(), &path).unwrap_err(),
        CompactionError::SnapshotConflict { sequence: 1 }
    );
    let manifest_after = Manifest::load_or_empty(&path.with_extension("manifest"), 906).unwrap();
    assert_eq!(manifest_after.entries().len(), 2);
    assert!(sstable_dir.join("left-v1.sst").exists());
    assert!(sstable_dir.join("right-v2.sst").exists());

    cleanup(&path);
}

#[test]
fn compaction_rejects_source_outside_descriptor_range_before_manifest_replacement() {
    let path = temp_wal("out-of-range");
    cleanup(&path);

    install_v1_manifest(
        &path,
        "out-of-range-v1.sst",
        &[TabletSnapshotRow {
            key: b"zz".to_vec(),
            versions: vec![VersionedValue {
                sequence: 1,
                value: Some(b"outside".to_vec()),
            }],
        }],
    );

    assert_eq!(
        compact_tablet_sstables_to_v2(&descriptor(), &path).unwrap_err(),
        CompactionError::ManifestSstableMismatch("out-of-range-v1.sst".to_owned())
    );
    let manifest_after = Manifest::load_or_empty(&path.with_extension("manifest"), 906).unwrap();
    assert_eq!(manifest_after.entries().len(), 1);
    assert_eq!(manifest_after.entries()[0].file_name, "out-of-range-v1.sst");
    assert!(path
        .with_extension("sstables")
        .join("out-of-range-v1.sst")
        .exists());

    cleanup(&path);
}
