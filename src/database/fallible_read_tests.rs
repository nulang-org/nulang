use super::sstable::{SstableError};
use super::sstable_indexed::{IndexedSstable, IndexedVersion};
use super::store::{WalBackedError, WalBackedTablet};

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
