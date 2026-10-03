use super::compaction::reclaim_orphan_sstables;
use super::manifest::{Manifest, ManifestEntry, SstableFormat, SstableIntegrity};
use super::sstable_v2::write_sstable_v2;
use super::tablet::{KeyRange, TabletDescriptor, TabletId, TabletSnapshotRow, VersionedValue};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_TEST: AtomicU64 = AtomicU64::new(1);

fn temp_wal(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "nulang_nudb_reclaim_{name}_{}_{}.wal",
        std::process::id(),
        NEXT_TEST.fetch_add(1, Ordering::Relaxed)
    ))
}

fn descriptor() -> TabletDescriptor {
    TabletDescriptor::new(
        TabletId::new(907).unwrap(),
        KeyRange::new(Vec::new(), None).unwrap(),
        1,
    )
    .unwrap()
}

fn cleanup(path: &Path) {
    let _ = fs::remove_file(path.with_extension("manifest"));
    let _ = fs::remove_dir_all(path.with_extension("sstables"));
}

fn rows(sequence: u64, value: &[u8]) -> Vec<TabletSnapshotRow> {
    vec![TabletSnapshotRow {
        key: b"k".to_vec(),
        versions: vec![VersionedValue {
            sequence,
            value: Some(value.to_vec()),
        }],
    }]
}

fn publish_v2_entry(path: &Path, file_name: &str) {
    let dir = path.with_extension("sstables");
    fs::create_dir_all(&dir).unwrap();
    let metadata = write_sstable_v2(&dir.join(file_name), 907, 1, &rows(1, b"live")).unwrap();
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
            format: SstableFormat::V2,
            integrity: SstableIntegrity::FooterBlake3(metadata.footer_checksum),
        })
        .unwrap();
    manifest.publish(&path.with_extension("manifest")).unwrap();
}

#[test]
fn reclaim_removes_only_unreferenced_sstable_files() {
    let path = temp_wal("basic");
    cleanup(&path);
    publish_v2_entry(&path, "live.sst");

    let dir = path.with_extension("sstables");
    write_sstable_v2(&dir.join("orphan-a.sst"), 907, 1, &rows(2, b"a")).unwrap();
    write_sstable_v2(&dir.join("orphan-b.sst"), 907, 1, &rows(3, b"b")).unwrap();
    fs::write(dir.join("notes.txt"), b"not a NuDB SSTable").unwrap();
    fs::create_dir(dir.join("nested.sst")).unwrap();

    assert_eq!(reclaim_orphan_sstables(&descriptor(), &path).unwrap(), 2);
    assert!(dir.join("live.sst").exists());
    assert!(!dir.join("orphan-a.sst").exists());
    assert!(!dir.join("orphan-b.sst").exists());
    assert!(dir.join("notes.txt").exists());
    assert!(dir.join("nested.sst").is_dir());

    cleanup(&path);
}

#[test]
fn reclaim_without_sstable_directory_is_a_noop() {
    let path = temp_wal("missing-dir");
    cleanup(&path);
    assert_eq!(reclaim_orphan_sstables(&descriptor(), &path).unwrap(), 0);
}

#[test]
fn corrupt_manifest_fails_before_any_orphan_is_deleted() {
    let path = temp_wal("corrupt-manifest");
    cleanup(&path);
    publish_v2_entry(&path, "live.sst");

    let dir = path.with_extension("sstables");
    write_sstable_v2(&dir.join("orphan.sst"), 907, 1, &rows(2, b"orphan")).unwrap();

    let manifest_path = path.with_extension("manifest");
    let mut bytes = fs::read(&manifest_path).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 0x40;
    fs::write(&manifest_path, bytes).unwrap();

    assert!(reclaim_orphan_sstables(&descriptor(), &path).is_err());
    assert!(dir.join("orphan.sst").exists());
    assert!(dir.join("live.sst").exists());

    cleanup(&path);
}
