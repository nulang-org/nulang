use super::sstable_v2::{write_sstable_v2, SstableV2, SstableV2Error};
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
    let first = table.version_at(b"key-0000", 1).unwrap().unwrap();
    assert_eq!(first.sequence, 1);
    assert_eq!(first.value.unwrap(), &[b'x'; 64]);

    let repeated = table.version_at(b"key-0000", 1).unwrap().unwrap();
    assert_eq!(
        first.value.unwrap().as_ptr(),
        repeated.value.unwrap().as_ptr(),
        "v2 point reads should borrow the immutable backing rather than allocate values"
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
fn test_v2_snapshot_rows_reconstruct_complete_mvcc_history() {
    let path = temp_path("snapshot-rows");
    let _ = fs::remove_file(&path);
    let rows = vec![
        TabletSnapshotRow {
            key: b"alpha".to_vec(),
            versions: vec![
                VersionedValue {
                    sequence: 1,
                    value: Some(b"one".to_vec()),
                },
                VersionedValue {
                    sequence: 3,
                    value: Some(b"three".to_vec()),
                },
                VersionedValue {
                    sequence: 5,
                    value: None,
                },
            ],
        },
        TabletSnapshotRow {
            key: b"beta".to_vec(),
            versions: vec![
                VersionedValue {
                    sequence: 2,
                    value: Some(b"two".to_vec()),
                },
                VersionedValue {
                    sequence: 4,
                    value: Some(b"four".to_vec()),
                },
            ],
        },
    ];

    write_sstable_v2(&path, 42, 7, &rows).unwrap();
    let table = SstableV2::open(&path).unwrap();
    assert_eq!(table.snapshot_rows().unwrap(), rows);

    let _ = fs::remove_file(path);
}

#[test]
fn test_v2_snapshot_rows_validate_every_block_before_checkpoint_use() {
    let path = temp_path("snapshot-corruption");
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
        table.snapshot_rows().unwrap_err(),
        SstableV2Error::BlockChecksumMismatch(last_block)
    );

    let _ = fs::remove_file(path);
}

#[cfg(unix)]
#[test]
fn test_v2_mmap_survives_unlink_and_still_serves_borrowed_values() {
    let path = temp_path("mmap-unlink");
    let _ = fs::remove_file(&path);
    write_sstable_v2(&path, 42, 7, &many_rows()).unwrap();

    let table = SstableV2::open(&path).unwrap();
    assert!(table.is_memory_mapped_for_test());
    fs::remove_file(&path).unwrap();

    let version = table.version_at(b"key-0512", 513).unwrap().unwrap();
    assert_eq!(version.sequence, 513);
    assert_eq!(version.value.unwrap(), &[b'x'; 64]);
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
