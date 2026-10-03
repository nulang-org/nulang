use super::manifest::{Manifest, ManifestEntry, SstableFormat, SstableIntegrity};
use super::sstable;
use super::store::WalBackedTablet;
use super::tablet::{
    KeyRange, TabletDescriptor, TabletId, TabletMutation, TabletSnapshotRow, VersionedValue,
};
use super::wal::FileWal;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_TEST: AtomicU64 = AtomicU64::new(1);

fn temp_wal(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "nulang_nudb_v2_serving_{name}_{}_{}.wal",
        std::process::id(),
        NEXT_TEST.fetch_add(1, Ordering::Relaxed)
    ))
}

fn descriptor() -> TabletDescriptor {
    TabletDescriptor::new(
        TabletId::new(905).unwrap(),
        KeyRange::new(b"a".to_vec(), Some(b"z".to_vec())).unwrap(),
        1,
    )
    .unwrap()
}

fn cleanup(path: &PathBuf) {
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

#[test]
fn new_flushes_publish_v2_and_remain_checkpoint_safe() {
    let path = temp_wal("new-flush");
    cleanup(&path);

    let mut tablet = WalBackedTablet::open(descriptor(), &path).unwrap();
    commit_put(&mut tablet, b"a", b"from-v2");
    let bytes = tablet.mutable_memtable_bytes();
    assert!(tablet.rotate_memtable_if_bytes_at_least(bytes));
    assert!(tablet.flush_oldest_immutable_to_sstable().unwrap());

    let manifest_bytes = fs::read(path.with_extension("manifest")).unwrap();
    assert_eq!(&manifest_bytes[..8], b"NUDBMAN2");
    let table_path = fs::read_dir(path.with_extension("sstables"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let table_bytes = fs::read(&table_path).unwrap();
    assert_eq!(&table_bytes[..8], b"NUDBSST2");
    assert!(table_path
        .file_name()
        .unwrap()
        .to_string_lossy()
        .contains("-v2-"));
    assert_eq!(tablet.read_latest(b"a").unwrap(), Some(&b"from-v2"[..]));

    tablet.checkpoint().unwrap();
    drop(tablet);
    fs::remove_file(path.with_extension("manifest")).unwrap();
    fs::remove_dir_all(path.with_extension("sstables")).unwrap();

    let reopened = WalBackedTablet::open(descriptor(), &path).unwrap();
    assert_eq!(reopened.current_sequence(), 1);
    assert_eq!(reopened.read_latest(b"a").unwrap(), Some(&b"from-v2"[..]));

    cleanup(&path);
}

#[test]
fn explicit_v1_manifest_entries_remain_recoverable_after_v2_activation() {
    let path = temp_wal("v1-compat");
    cleanup(&path);

    {
        let mut tablet = WalBackedTablet::open(descriptor(), &path).unwrap();
        commit_put(&mut tablet, b"k", b"legacy-v1");
    }

    let rows = vec![TabletSnapshotRow {
        key: b"k".to_vec(),
        versions: vec![VersionedValue {
            sequence: 1,
            value: Some(b"legacy-v1".to_vec()),
        }],
    }];
    let sstable_dir = path.with_extension("sstables");
    fs::create_dir_all(&sstable_dir).unwrap();
    let table_path = sstable_dir.join("legacy-v1.sst");
    let metadata = sstable::write_sstable(&table_path, 905, 1, &rows).unwrap();

    let mut manifest = Manifest::empty(905);
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

    let mut wal = FileWal::open(&path).unwrap();
    wal.reclaim_through(1).unwrap();
    drop(wal);

    let reopened = WalBackedTablet::open(descriptor(), &path).unwrap();
    assert_eq!(reopened.current_sequence(), 1);
    assert_eq!(
        reopened.read_latest(b"k").unwrap(),
        Some(&b"legacy-v1"[..])
    );

    cleanup(&path);
}
