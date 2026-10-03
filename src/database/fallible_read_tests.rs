use super::sstable::SstableError;
use super::sstable_indexed::{IndexedSstable, IndexedVersion};
use super::store::{WalBackedError, WalBackedTablet};
use super::tablet::{KeyRange, TabletDescriptor, TabletError, TabletId};
use std::fs;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_TEST: AtomicU64 = AtomicU64::new(1);

#[test]
fn test_durable_read_contracts_are_fallible() {
    fn assert_indexed_signature<'a>(
        table: &'a IndexedSstable,
    ) -> Result<Option<IndexedVersion<'a>>, SstableError> {
        table.version_at(b"k", 0)
    }

    fn assert_read_at_signature<'a>(
        tablet: &'a WalBackedTablet,
    ) -> Result<Option<&'a [u8]>, WalBackedError> {
        tablet.read_at(b"k", 0)
    }

    fn assert_read_latest_signature<'a>(
        tablet: &'a WalBackedTablet,
    ) -> Result<Option<&'a [u8]>, WalBackedError> {
        tablet.read_latest(b"k")
    }

    let _ = assert_indexed_signature;
    let _ = assert_read_at_signature;
    let _ = assert_read_latest_signature;
}

#[test]
fn test_latest_read_distinguishes_wrong_tablet_from_absent_key() {
    let path = std::env::temp_dir().join(format!(
        "nulang_nudb_read_range_{}_{}.wal",
        std::process::id(),
        NEXT_TEST.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_file(&path);

    let descriptor = TabletDescriptor::new(
        TabletId::new(908).unwrap(),
        KeyRange::new(b"m".to_vec(), Some(b"z".to_vec())).unwrap(),
        1,
    )
    .unwrap();
    let tablet = WalBackedTablet::open(descriptor, &path).unwrap();

    assert_eq!(
        tablet.read_latest(b"n").unwrap(),
        None,
        "an in-range missing key must remain an ordinary miss"
    );
    assert_eq!(
        tablet.read_latest(b"a").unwrap_err(),
        WalBackedError::Tablet(TabletError::KeyOutsideTabletRange),
        "an out-of-range key must tell the router it reached the wrong tablet"
    );

    drop(tablet);
    let _ = fs::remove_file(path);
}
