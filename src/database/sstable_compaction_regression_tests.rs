use super::compaction::{compact_tablet_sstables_to_v2, CompactionError};
use super::manifest::{Manifest, ManifestEntry, SstableFormat, SstableIntegrity};
use super::tablet::{KeyRange, TabletDescriptor, TabletId};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_TEST: AtomicU64 = AtomicU64::new(1);

fn temp_wal(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "nulang_nudb_compaction_regression_{name}_{}_{}.wal",
        std::process::id(),
        NEXT_TEST.fetch_add(1, Ordering::Relaxed)
    ))
}

fn descriptor() -> TabletDescriptor {
    TabletDescriptor::new(
        TabletId::new(908).unwrap(),
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

#[test]
fn single_v2_noop_still_validates_manifest_authority() {
    let path = temp_wal("single-v2-missing");
    cleanup(&path);

    let mut manifest = Manifest::empty(908);
    manifest
        .register(ManifestEntry {
            file_name: "missing-v2.sst".to_owned(),
            tablet_id: 908,
            ownership_epoch: 1,
            min_sequence: 1,
            max_sequence: 1,
            row_count: 1,
            min_key: b"k".to_vec(),
            max_key: b"k".to_vec(),
            format: SstableFormat::V2,
            integrity: SstableIntegrity::FooterBlake3([7; 32]),
        })
        .unwrap();
    manifest.publish(&path.with_extension("manifest")).unwrap();

    assert!(matches!(
        compact_tablet_sstables_to_v2(&descriptor(), &path),
        Err(CompactionError::SstableV2(_))
    ));

    let after = Manifest::load_or_empty(&path.with_extension("manifest"), 908).unwrap();
    assert_eq!(after.entries().len(), 1);
    assert_eq!(after.entries()[0].file_name, "missing-v2.sst");

    cleanup(&path);
}
