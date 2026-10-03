use super::manifest::{Manifest, ManifestEntry, SstableFormat, SstableIntegrity};
use super::sstable;
use super::sstable_v2::write_sstable_v2;
use super::store::{WalBackedError, WalBackedTablet};
use super::tablet::{KeyRange, TabletDescriptor, TabletId, TabletSnapshotRow, VersionedValue};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_TEST: AtomicU64 = AtomicU64::new(1);

fn temp_wal(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "nulang_nudb_v2_serving_regression_{name}_{}_{}.wal",
        std::process::id(),
        NEXT_TEST.fetch_add(1, Ordering::Relaxed)
    ))
}

fn descriptor() -> TabletDescriptor {
    TabletDescriptor::new(
        TabletId::new(907).unwrap(),
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

fn row(key: &[u8], sequence: u64, value: &[u8]) -> Vec<TabletSnapshotRow> {
    vec![TabletSnapshotRow {
        key: key.to_vec(),
        versions: vec![VersionedValue {
            sequence,
            value: Some(value.to_vec()),
        }],
    }]
}

#[test]
fn v1_manifest_entry_outside_descriptor_range_fails_closed() {
    let path = temp_wal("v1-out-of-range");
    cleanup(&path);

    let sstable_dir = path.with_extension("sstables");
    fs::create_dir_all(&sstable_dir).unwrap();
    let table_path = sstable_dir.join("outside-v1.sst");
    let metadata = sstable::write_sstable(&table_path, 907, 1, &row(b"zz", 1, b"outside"))
        .unwrap();

    let mut manifest = Manifest::empty(907);
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

    assert!(matches!(
        WalBackedTablet::open(descriptor(), &path),
        Err(WalBackedError::ManifestSstableMismatch(file)) if file == "outside-v1.sst"
    ));
    assert!(table_path.exists());

    cleanup(&path);
}

#[test]
fn point_read_rejects_conflicting_equal_sequence_across_serving_tables() {
    let path = temp_wal("equal-sequence-conflict");
    cleanup(&path);

    let sstable_dir = path.with_extension("sstables");
    fs::create_dir_all(&sstable_dir).unwrap();

    let left_path = sstable_dir.join("left-v1.sst");
    let left = sstable::write_sstable(&left_path, 907, 1, &row(b"k", 1, b"left")).unwrap();
    let right_path = sstable_dir.join("right-v2.sst");
    let right = write_sstable_v2(&right_path, 907, 1, &row(b"k", 1, b"right")).unwrap();

    let mut manifest = Manifest::empty(907);
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
    manifest.publish(&path.with_extension("manifest")).unwrap();

    let tablet = WalBackedTablet::open(descriptor(), &path).unwrap();
    assert_eq!(
        tablet.read_at(b"k", 1).unwrap_err(),
        WalBackedError::ReadConflict { sequence: 1 }
    );

    cleanup(&path);
}
