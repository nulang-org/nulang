use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use nulang::database::store::{WalBackedError, WalBackedTablet};
use nulang::database::tablet::{KeyRange, TabletDescriptor, TabletId, TabletMutation};
use nulang::database::wal::FileWal;

static NEXT_TEST: AtomicU64 = AtomicU64::new(1);

fn temp_wal(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "nulang_nudb_sstable_recovery_{name}_{}_{}.wal",
        std::process::id(),
        NEXT_TEST.fetch_add(1, Ordering::Relaxed)
    ))
}

fn descriptor() -> TabletDescriptor {
    TabletDescriptor::new(
        TabletId::new(904).unwrap(),
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

fn flush_current(tablet: &mut WalBackedTablet) {
    let bytes = tablet.mutable_memtable_bytes();
    assert!(bytes > 0);
    assert!(tablet.rotate_memtable_if_bytes_at_least(bytes));
    assert!(tablet.flush_oldest_immutable_to_sstable().unwrap());
}

#[test]
fn manifest_sstable_recovers_when_wal_prefix_has_been_reclaimed() {
    let path = temp_wal("reclaimed");
    cleanup(&path);
    let mut tablet = WalBackedTablet::open(descriptor(), &path).unwrap();
    commit_put(&mut tablet, b"k", b"v1");
    flush_current(&mut tablet);
    drop(tablet);

    let mut wal = FileWal::open(&path).unwrap();
    wal.reclaim_through(1).unwrap();
    drop(wal);

    let reopened = WalBackedTablet::open(descriptor(), &path).unwrap();
    assert_eq!(reopened.current_sequence(), 1);
    assert_eq!(reopened.read_latest(b"k").unwrap(), Some(&b"v1"[..]));
    cleanup(&path);
}

#[test]
fn sstable_baseline_and_newer_wal_records_compose_without_duplicate_history() {
    let path = temp_wal("wal_tail");
    cleanup(&path);
    let mut tablet = WalBackedTablet::open(descriptor(), &path).unwrap();
    commit_put(&mut tablet, b"k", b"v1");
    flush_current(&mut tablet);
    commit_put(&mut tablet, b"k", b"v2");
    drop(tablet);

    let reopened = WalBackedTablet::open(descriptor(), &path).unwrap();
    assert_eq!(reopened.current_sequence(), 2);
    assert_eq!(reopened.read_at(b"k", 1).unwrap(), Some(&b"v1"[..]));
    assert_eq!(reopened.read_at(b"k", 2).unwrap(), Some(&b"v2"[..]));
    cleanup(&path);
}

#[test]
fn corrupt_manifest_referenced_sstable_fails_reopen_closed() {
    let path = temp_wal("corrupt_table");
    cleanup(&path);
    let mut tablet = WalBackedTablet::open(descriptor(), &path).unwrap();
    commit_put(&mut tablet, b"k", b"v1");
    flush_current(&mut tablet);
    drop(tablet);

    let dir = path.with_extension("sstables");
    let table_path = fs::read_dir(&dir).unwrap().next().unwrap().unwrap().path();
    let mut bytes = fs::read(&table_path).unwrap();
    bytes[20] ^= 0x40;
    fs::write(&table_path, bytes).unwrap();

    assert!(matches!(
        WalBackedTablet::open(descriptor(), &path),
        Err(WalBackedError::Sstable(_)) | Err(WalBackedError::SstableV2(_))
    ));
    cleanup(&path);
}

#[test]
fn checkpoint_overlap_uses_only_sstable_versions_newer_than_checkpoint() {
    let path = temp_wal("checkpoint_overlap");
    cleanup(&path);
    let mut tablet = WalBackedTablet::open(descriptor(), &path).unwrap();
    commit_put(&mut tablet, b"k", b"v1");
    tablet.publish_checkpoint().unwrap();
    commit_put(&mut tablet, b"k", b"v2");
    flush_current(&mut tablet);
    drop(tablet);

    let reopened = WalBackedTablet::open(descriptor(), &path).unwrap();
    assert_eq!(reopened.current_sequence(), 2);
    assert_eq!(reopened.read_at(b"k", 1).unwrap(), Some(&b"v1"[..]));
    assert_eq!(reopened.read_at(b"k", 2).unwrap(), Some(&b"v2"[..]));
    cleanup(&path);
}

#[test]
fn sstable_does_not_claim_unrepresented_sequence_coverage_after_wal_reclaim() {
    let path = temp_wal("sequence_gap");
    cleanup(&path);
    let mut tablet = WalBackedTablet::open(descriptor(), &path).unwrap();
    commit_put(&mut tablet, b"k", b"v1");

    let empty = tablet.prepare_write(1, 1, vec![]).unwrap();
    tablet.commit(empty).unwrap();
    commit_put(&mut tablet, b"k", b"v3");
    flush_current(&mut tablet);
    drop(tablet);

    let mut wal = FileWal::open(&path).unwrap();
    wal.reclaim_through(3).unwrap();
    drop(wal);

    assert!(matches!(
        WalBackedTablet::open(descriptor(), &path),
        Err(WalBackedError::Wal(_))
    ));
    cleanup(&path);
}
