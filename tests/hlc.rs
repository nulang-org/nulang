use nulang::hlc::{HlcError, HlcTimestamp, HybridLogicalClock};

#[test]
fn physical_time_advance_resets_logical_counter() {
    let mut clock = HybridLogicalClock::new(5_000);

    let first = clock.tick(1_000).unwrap();
    let second = clock.tick(2_000).unwrap();

    assert_eq!(first, HlcTimestamp::new(1_000, 0));
    assert_eq!(second, HlcTimestamp::new(2_000, 0));
}

#[test]
fn local_clock_rollback_preserves_monotonicity_with_logical_counter() {
    let mut clock = HybridLogicalClock::new(5_000);

    let first = clock.tick(2_000).unwrap();
    let second = clock.tick(1_500).unwrap();
    let third = clock.tick(1_500).unwrap();

    assert_eq!(first, HlcTimestamp::new(2_000, 0));
    assert_eq!(second, HlcTimestamp::new(2_000, 1));
    assert_eq!(third, HlcTimestamp::new(2_000, 2));
    assert!(first < second && second < third);
}

#[test]
fn observing_remote_time_advances_past_remote_timestamp() {
    let mut clock = HybridLogicalClock::new(5_000);
    clock.tick(1_000).unwrap();

    let observed = clock
        .observe(HlcTimestamp::new(1_500, 7), 1_100)
        .unwrap();

    assert_eq!(observed, HlcTimestamp::new(1_500, 8));
}

#[test]
fn observing_equal_physical_time_uses_greatest_logical_counter() {
    let mut clock = HybridLogicalClock::new(5_000);
    clock.observe(HlcTimestamp::new(2_000, 4), 2_000).unwrap();

    let observed = clock
        .observe(HlcTimestamp::new(2_000, 9), 1_900)
        .unwrap();

    assert_eq!(observed, HlcTimestamp::new(2_000, 10));
}

#[test]
fn remote_clock_beyond_configured_future_drift_fails_closed() {
    let mut clock = HybridLogicalClock::new(250);
    let before = clock.tick(1_000).unwrap();

    let error = clock
        .observe(HlcTimestamp::new(1_251, 0), 1_000)
        .unwrap_err();

    assert_eq!(
        error,
        HlcError::RemoteClockTooFarAhead {
            remote_physical_micros: 1_251,
            local_physical_micros: 1_000,
            max_future_drift_micros: 250,
        }
    );
    assert_eq!(clock.last(), before);
}

#[test]
fn logical_counter_overflow_fails_without_advancing_clock() {
    let mut clock = HybridLogicalClock::new(u64::MAX);
    clock
        .observe(HlcTimestamp::new(5_000, u32::MAX - 1), 5_000)
        .unwrap();
    let before = clock.last();

    let error = clock.tick(4_000).unwrap_err();

    assert_eq!(
        error,
        HlcError::LogicalOverflow {
            physical_micros: 5_000,
        }
    );
    assert_eq!(clock.last(), before);
}

#[test]
fn timestamp_order_is_lexicographic_and_serde_round_trips() {
    let earlier = HlcTimestamp::new(8_000, 12);
    let later_logical = HlcTimestamp::new(8_000, 13);
    let later_physical = HlcTimestamp::new(8_001, 0);

    assert!(earlier < later_logical);
    assert!(later_logical < later_physical);

    let json = serde_json::to_string(&later_logical).unwrap();
    let decoded: HlcTimestamp = serde_json::from_str(&json).unwrap();
    assert_eq!(decoded, later_logical);
}
