use nulang_security::RevocationEpoch;

#[test]
fn revocation_epoch_advances_monotonically() {
    let epoch = RevocationEpoch::new(7);
    let next = epoch.next().expect("epoch should advance");

    assert_eq!(next, RevocationEpoch::new(8));
    assert!(next > epoch);
}

#[test]
fn max_revocation_epoch_does_not_wrap() {
    assert_eq!(RevocationEpoch::new(u64::MAX).next(), None);
}
