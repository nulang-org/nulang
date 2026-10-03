use super::sstable_v2::{
    write_sstable_v2, write_sstable_v2_with_coverage, OwnedVersion, SequenceCoverageRun, SstableV2,
    SstableV2Error,
};
use super::tablet::{TabletSnapshotRow, VersionedValue};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_TEST: AtomicU64 = AtomicU64::new(1);

fn temp_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "nulang_nudb_sstable_v2_{name}_{}_{}.sst",
        std::process::id(),
        NEXT_TEST.fetch_add(1, Ordering::Relaxed)
    ))
}

fn many_rows() -> Vec<TabletSnapshotRow> {
    (0_u64..1024)
        .map(|index| TabletSnapshotRow {
            key: format!("key-{index:04}").into_bytes(),
            versions: vec![VersionedValue {
                sequence: index + 1,
                value: Some(vec![b'x'; 64]),
            }],
        })
        .collect()
}

#[test]
fn test_v2_footer_metadata_and_point_reads_match_written_rows() {
    let path = temp_path("roundtrip");
    let _ = fs::remove_file(&path);
    let rows = many_rows();

    let written = write_sstable_v2(&path, 42, 7, &rows).unwrap();
    let table = SstableV2::open(&path).unwrap();

    assert_eq!(table.metadata(), &written);
    assert!(table.block_count_for_test() > 1);
    assert_eq!(
        table.version_at(b"key-0000", 1).unwrap(),
        Some(OwnedVersion {
            sequence: 1,
            value: Some(vec![b'x'; 64]),
        })
    );
    assert_eq!(
        table
            .version_at(b"key-1023", 1024)
            .unwrap()
            .unwrap()
            .sequence,
        1024
    );

    let _ = fs::remove_file(path);
}

#[test]
fn test_v2_corrupt_block_is_detected_lazily_without_poisoning_other_blocks() {
    let path = temp_path("block-corruption");
    let _ = fs::remove_file(&path);
    let rows = many_rows();
    write_sstable_v2(&path, 42, 7, &rows).unwrap();

    let table = SstableV2::open(&path).unwrap();
    let last_block = table.block_count_for_test() - 1;
    let corrupt_range = table.block_payload_range_for_test(last_block).unwrap();
    drop(table);

    let mut bytes = fs::read(&path).unwrap();
    bytes[corrupt_range.start] ^= 0x40;
    fs::write(&path, bytes).unwrap();

    let table = SstableV2::open(&path).unwrap();
    assert_eq!(
        table.version_at(b"key-0000", 1).unwrap().unwrap().sequence,
        1
    );
    assert_eq!(
        table.version_at(b"key-1023", 1024).unwrap_err(),
        SstableV2Error::BlockChecksumMismatch(last_block)
    );

    let _ = fs::remove_file(path);
}

#[test]
fn test_v2_footer_corruption_fails_during_open() {
    let path = temp_path("footer-corruption");
    let _ = fs::remove_file(&path);
    write_sstable_v2(&path, 42, 7, &many_rows()).unwrap();

    let table = SstableV2::open(&path).unwrap();
    let footer = table.footer_range_for_test();
    drop(table);

    let mut bytes = fs::read(&path).unwrap();
    bytes[footer.start] ^= 0x20;
    fs::write(&path, bytes).unwrap();

    assert_eq!(
        SstableV2::open(&path).unwrap_err(),
        SstableV2Error::FooterChecksumMismatch
    );

    let _ = fs::remove_file(path);
}

#[test]
fn test_v2_footer_preserves_recovery_sequence_coverage_without_block_scan() {
    let path = temp_path("coverage");
    let _ = fs::remove_file(&path);
    let rows = vec![
        TabletSnapshotRow {
            key: b"alpha".to_vec(),
            versions: vec![VersionedValue {
                sequence: 1,
                value: Some(b"one".to_vec()),
            }],
        },
        TabletSnapshotRow {
            key: b"beta".to_vec(),
            versions: vec![VersionedValue {
                sequence: 3,
                value: Some(b"three".to_vec()),
            }],
        },
    ];
    write_sstable_v2(&path, 42, 7, &rows).unwrap();
    let table = SstableV2::open(&path).unwrap();

    assert!(!table.has_contiguous_sequence_coverage_after(0));
    assert!(table.has_contiguous_sequence_coverage_after(2));
    assert!(table.has_contiguous_sequence_coverage_after(3));

    let _ = fs::remove_file(path);
}

#[test]
fn test_v2_explicit_coverage_can_outlive_pruned_mvcc_versions() {
    let path = temp_path("gc-coverage");
    let _ = fs::remove_file(&path);
    let rows = vec![
        TabletSnapshotRow {
            key: b"alpha".to_vec(),
            versions: vec![VersionedValue {
                sequence: 3,
                value: None,
            }],
        },
        TabletSnapshotRow {
            key: b"beta".to_vec(),
            versions: vec![VersionedValue {
                sequence: 5,
                value: Some(b"five".to_vec()),
            }],
        },
    ];
    let coverage = [SequenceCoverageRun::new(1, 5).unwrap()];

    let written = write_sstable_v2_with_coverage(&path, 42, 7, &rows, &coverage).unwrap();
    assert_eq!(written.min_sequence, 1);
    assert_eq!(written.max_sequence, 5);

    let table = SstableV2::open(&path).unwrap();
    assert!(table.has_contiguous_sequence_coverage_after(0));
    assert_eq!(table.version_at(b"alpha", 2).unwrap(), None);
    assert_eq!(
        table.version_at(b"alpha", 3).unwrap(),
        Some(OwnedVersion {
            sequence: 3,
            value: None,
        })
    );
    assert_eq!(
        table.version_at(b"beta", 5).unwrap(),
        Some(OwnedVersion {
            sequence: 5,
            value: Some(b"five".to_vec()),
        })
    );

    let _ = fs::remove_file(path);
}

#[test]
fn test_v2_explicit_coverage_must_include_every_retained_version_sequence() {
    let path = temp_path("invalid-gc-coverage");
    let _ = fs::remove_file(&path);
    let rows = vec![TabletSnapshotRow {
        key: b"alpha".to_vec(),
        versions: vec![VersionedValue {
            sequence: 3,
            value: Some(b"three".to_vec()),
        }],
    }];
    let coverage = [SequenceCoverageRun::new(1, 2).unwrap()];

    assert_eq!(
        write_sstable_v2_with_coverage(&path, 42, 7, &rows, &coverage).unwrap_err(),
        SstableV2Error::SequenceNotCovered(3)
    );
    assert!(!path.exists());
}

#[test]
fn test_v2_sequence_coverage_runs_are_nonzero_ordered_and_nonoverlapping() {
    assert_eq!(
        SequenceCoverageRun::new(0, 2).unwrap_err(),
        SstableV2Error::InvalidSequenceCoverage
    );
    assert_eq!(
        SequenceCoverageRun::new(4, 3).unwrap_err(),
        SstableV2Error::InvalidSequenceCoverage
    );

    let path = temp_path("overlap-coverage");
    let _ = fs::remove_file(&path);
    let rows = vec![TabletSnapshotRow {
        key: b"alpha".to_vec(),
        versions: vec![VersionedValue {
            sequence: 3,
            value: Some(b"three".to_vec()),
        }],
    }];
    let coverage = [
        SequenceCoverageRun::new(1, 3).unwrap(),
        SequenceCoverageRun::new(3, 5).unwrap(),
    ];
    assert_eq!(
        write_sstable_v2_with_coverage(&path, 42, 7, &rows, &coverage).unwrap_err(),
        SstableV2Error::InvalidSequenceCoverage
    );
    assert!(!path.exists());
}
