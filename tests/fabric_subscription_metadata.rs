#[path = "../src/runtime/fabric_subscription_metadata.rs"]
mod fabric_subscription_metadata;

use fabric_subscription_metadata::{
    decode_subscription_metadata, encode_subscription_metadata, FabricSubscriptionMetadataEntry,
    FabricSubscriptionMetadataError,
};

const MAX_SUBSCRIPTIONS: usize = 1024;
const MAX_SNAPSHOT_BYTES: usize = 256 * 1024;
const MAX_STRING_BYTES: usize = 4096;

fn entry(
    pattern: &str,
    actor_id: u64,
    behavior: &str,
    group: Option<&str>,
) -> FabricSubscriptionMetadataEntry {
    FabricSubscriptionMetadataEntry {
        pattern: pattern.to_string(),
        actor_id,
        behavior: behavior.to_string(),
        group: group.map(str::to_string),
    }
}

#[test]
fn subscription_metadata_roundtrip_preserves_order_and_utf8() {
    let subscriptions = vec![
        entry("orders.*", 7, "handle", Some("workers")),
        entry("tenant.日本.>", 9, "consume_✓", None),
    ];

    let encoded = encode_subscription_metadata(
        &subscriptions,
        MAX_SUBSCRIPTIONS,
        MAX_SNAPSHOT_BYTES,
        MAX_STRING_BYTES,
    )
    .unwrap();
    assert_eq!(&encoded[..4], b"FAS1");

    let decoded = decode_subscription_metadata(
        &encoded,
        MAX_SUBSCRIPTIONS,
        MAX_SNAPSHOT_BYTES,
        MAX_STRING_BYTES,
    )
    .unwrap();
    assert_eq!(decoded, subscriptions);
}

#[test]
fn empty_subscription_snapshot_is_authoritative_and_roundtrips() {
    let encoded = encode_subscription_metadata(
        &[],
        MAX_SUBSCRIPTIONS,
        MAX_SNAPSHOT_BYTES,
        MAX_STRING_BYTES,
    )
    .unwrap();
    assert_eq!(encoded, b"FAS1\0\0\0\0");

    let decoded = decode_subscription_metadata(
        &encoded,
        MAX_SUBSCRIPTIONS,
        MAX_SNAPSHOT_BYTES,
        MAX_STRING_BYTES,
    )
    .unwrap();
    assert!(decoded.is_empty());
}

#[test]
fn decoder_rejects_declared_subscription_count_before_allocating_entries() {
    let mut encoded = b"FAS1".to_vec();
    encoded.extend_from_slice(&((MAX_SUBSCRIPTIONS as u32) + 1).to_be_bytes());

    assert_eq!(
        decode_subscription_metadata(
            &encoded,
            MAX_SUBSCRIPTIONS,
            MAX_SNAPSHOT_BYTES,
            MAX_STRING_BYTES,
        ),
        Err(FabricSubscriptionMetadataError::TooManySubscriptions)
    );
}

#[test]
fn codec_enforces_snapshot_and_string_bounds() {
    let subscriptions = vec![entry("orders.created", 1, "handle", None)];
    let encoded = encode_subscription_metadata(
        &subscriptions,
        MAX_SUBSCRIPTIONS,
        MAX_SNAPSHOT_BYTES,
        MAX_STRING_BYTES,
    )
    .unwrap();

    assert_eq!(
        encode_subscription_metadata(
            &subscriptions,
            MAX_SUBSCRIPTIONS,
            encoded.len() - 1,
            MAX_STRING_BYTES,
        ),
        Err(FabricSubscriptionMetadataError::SnapshotTooLarge)
    );
    assert_eq!(
        decode_subscription_metadata(
            &encoded,
            MAX_SUBSCRIPTIONS,
            encoded.len() - 1,
            MAX_STRING_BYTES,
        ),
        Err(FabricSubscriptionMetadataError::SnapshotTooLarge)
    );

    let long = vec![entry("orders.created", 1, "handle-too-long", None)];
    assert_eq!(
        encode_subscription_metadata(&long, MAX_SUBSCRIPTIONS, MAX_SNAPSHOT_BYTES, 4),
        Err(FabricSubscriptionMetadataError::StringTooLong)
    );

    assert_eq!(
        decode_subscription_metadata(&encoded, MAX_SUBSCRIPTIONS, MAX_SNAPSHOT_BYTES, 5),
        Err(FabricSubscriptionMetadataError::StringTooLong)
    );
}

#[test]
fn decoder_rejects_truncation_without_partial_output() {
    let encoded = encode_subscription_metadata(
        &[entry("orders.created", 11, "handle", Some("workers"))],
        MAX_SUBSCRIPTIONS,
        MAX_SNAPSHOT_BYTES,
        MAX_STRING_BYTES,
    )
    .unwrap();

    for cut in 0..encoded.len() {
        assert_eq!(
            decode_subscription_metadata(
                &encoded[..cut],
                MAX_SUBSCRIPTIONS,
                MAX_SNAPSHOT_BYTES,
                MAX_STRING_BYTES,
            ),
            Err(FabricSubscriptionMetadataError::Truncated),
            "cut={cut}"
        );
    }
}

#[test]
fn decoder_rejects_bad_magic_invalid_group_flag_and_trailing_bytes() {
    let encoded = encode_subscription_metadata(
        &[entry("orders.created", 17, "handle", None)],
        MAX_SUBSCRIPTIONS,
        MAX_SNAPSHOT_BYTES,
        MAX_STRING_BYTES,
    )
    .unwrap();

    let mut bad_magic = encoded.clone();
    bad_magic[0..4].copy_from_slice(b"NOPE");
    assert_eq!(
        decode_subscription_metadata(
            &bad_magic,
            MAX_SUBSCRIPTIONS,
            MAX_SNAPSHOT_BYTES,
            MAX_STRING_BYTES,
        ),
        Err(FabricSubscriptionMetadataError::BadMagic)
    );

    let mut bad_group = encoded.clone();
    *bad_group.last_mut().unwrap() = 2;
    assert_eq!(
        decode_subscription_metadata(
            &bad_group,
            MAX_SUBSCRIPTIONS,
            MAX_SNAPSHOT_BYTES,
            MAX_STRING_BYTES,
        ),
        Err(FabricSubscriptionMetadataError::InvalidGroupFlag)
    );

    let mut trailing = encoded;
    trailing.push(0xAA);
    assert_eq!(
        decode_subscription_metadata(
            &trailing,
            MAX_SUBSCRIPTIONS,
            MAX_SNAPSHOT_BYTES,
            MAX_STRING_BYTES,
        ),
        Err(FabricSubscriptionMetadataError::TrailingBytes)
    );
}

#[test]
fn codec_rejects_invalid_subscription_identity_fields() {
    for invalid in [
        entry("", 1, "handle", None),
        entry("orders.created", 0, "handle", None),
        entry("orders.created", 1, "", None),
        entry("orders.created", 1, "handle", Some("")),
    ] {
        assert_eq!(
            encode_subscription_metadata(
                &[invalid],
                MAX_SUBSCRIPTIONS,
                MAX_SNAPSHOT_BYTES,
                MAX_STRING_BYTES,
            ),
            Err(FabricSubscriptionMetadataError::InvalidSubscription)
        );
    }
}

#[test]
fn encoding_is_deterministic_for_identical_ordered_snapshot() {
    let subscriptions = vec![
        entry("a.*", 1, "one", None),
        entry("b.>", 2, "two", Some("g")),
    ];
    let first = encode_subscription_metadata(
        &subscriptions,
        MAX_SUBSCRIPTIONS,
        MAX_SNAPSHOT_BYTES,
        MAX_STRING_BYTES,
    )
    .unwrap();
    let second = encode_subscription_metadata(
        &subscriptions,
        MAX_SUBSCRIPTIONS,
        MAX_SNAPSHOT_BYTES,
        MAX_STRING_BYTES,
    )
    .unwrap();
    assert_eq!(first, second);
}
