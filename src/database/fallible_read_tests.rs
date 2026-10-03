use super::store::{WalBackedError, WalBackedTablet};

#[test]
fn test_wal_backed_read_contract_is_fallible_for_snapshot_and_latest_reads() {
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

    let _ = assert_read_at_signature;
    let _ = assert_read_latest_signature;
}
