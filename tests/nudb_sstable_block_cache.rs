use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use nulang::database::sstable::SstableError;
use nulang::database::store::{WalBackedError, WalBackedTablet};
use nulang::database::tablet::{KeyRange, TabletDescriptor, TabletId, TabletMutation};

static NEXT_TEST: AtomicU64 = AtomicU64::new(1);

fn temp_wal(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "nulang_nudb_sstable_block_cache_{name}_{}_{}.wal",
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

fn populate_large_sstable(path: &PathBuf) {
    let mut tablet = WalBackedTablet::open(descriptor(), path).unwrap();
    let mutations = (0_u8..96)
        .map(|i| TabletMutation::Put {
            key: format!("k{i:03}").into_bytes(),
            value: vec![i; 2048],
        })
        .collect();
    let write = tablet.prepare_write(1, 0, mutations).unwrap();
    tablet.commit(write).unwrap();
    let bytes = tablet.mutable_memtable_bytes();
    assert!(tablet.rotate_memtable_if_bytes_at_least(bytes));
    assert!(tablet.flush_oldest_immutable_to_sstable().unwrap());
}

#[test]
fn reopened_sstable_starts_cold_and_cache_stays_within_one_tablet_budget() {
    let path = temp_wal("bounded");
    cleanup(&path);
    populate_large_sstable(&path);

    let cache_budget = 72 * 1024;
    let tablet = WalBackedTablet::open_with_sstable_cache_bytes(
        descriptor(),
        &path,
        cache_budget,
    )
    .unwrap();

    let initial = tablet.sstable_cache_stats().unwrap();
    assert_eq!(initial.max_bytes, cache_budget);
    assert_eq!(initial.resident_bytes, 0);
    assert_eq!(initial.entries, 0);

    assert_eq!(
        tablet.read_latest(b"k000").unwrap().as_deref(),
        Some(&vec![0_u8; 2048][..])
    );
    let after_first = tablet.sstable_cache_stats().unwrap();
    assert_eq!(after_first.misses, 1);
    assert!(after_first.entries >= 1);
    assert!(after_first.resident_bytes <= cache_budget);

    assert_eq!(
        tablet.read_latest(b"k000").unwrap().as_deref(),
        Some(&vec![0_u8; 2048][..])
    );
    let after_hit = tablet.sstable_cache_stats().unwrap();
    assert_eq!(after_hit.hits, after_first.hits + 1);
    assert_eq!(after_hit.misses, after_first.misses);
    assert!(after_hit.resident_bytes <= cache_budget);

    assert_eq!(
        tablet.read_latest(b"k095").unwrap().as_deref(),
        Some(&vec![95_u8; 2048][..])
    );
    let after_far_read = tablet.sstable_cache_stats().unwrap();
    assert!(after_far_read.misses > after_hit.misses);
    assert!(after_far_read.resident_bytes <= cache_budget);

    cleanup(&path);
}

#[test]
fn checkpoint_scan_does_not_pollute_the_serving_cache() {
    let path = temp_wal("checkpoint_scan");
    cleanup(&path);
    populate_large_sstable(&path);

    let tablet = WalBackedTablet::open_with_sstable_cache_bytes(
        descriptor(),
        &path,
        72 * 1024,
    )
    .unwrap();
    assert_eq!(
        tablet.read_latest(b"k000").unwrap().as_deref(),
        Some(&vec![0_u8; 2048][..])
    );
    let before = tablet.sstable_cache_stats().unwrap();

    tablet.publish_checkpoint().unwrap();

    let after = tablet.sstable_cache_stats().unwrap();
    assert_eq!(after, before, "checkpoint scans bypass the serving cache");
    cleanup(&path);
}

#[test]
fn post_open_sstable_corruption_fails_the_target_block_read_closed() {
    let path = temp_wal("post_open_corruption");
    cleanup(&path);
    populate_large_sstable(&path);

    let tablet = WalBackedTablet::open_with_sstable_cache_bytes(
        descriptor(),
        &path,
        72 * 1024,
    )
    .unwrap();

    let sstable_dir = path.with_extension("sstables");
    let sstable_path = fs::read_dir(&sstable_dir)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let mut bytes = fs::read(&sstable_path).unwrap();
    bytes[40] ^= 0x40;
    fs::write(&sstable_path, bytes).unwrap();

    assert!(matches!(
        tablet.read_latest(b"k000"),
        Err(WalBackedError::Sstable(SstableError::ChecksumMismatch))
    ));
    cleanup(&path);
}
