#[path = "../src/runtime/fabric_metadata.rs"]
mod fabric_metadata;
#[path = "../src/runtime/fabric_metadata_wire.rs"]
mod fabric_metadata_wire;

use fabric_metadata::{chunk_snapshot, FabricMetadataKind};
use fabric_metadata_wire::{decode_fab1_chunk, encode_fab1_chunk, FabricMetadataWireError};

#[test]
fn fab1_roundtrip_preserves_chunk_identity_and_payload() {
    let chunk = chunk_snapshot(
        FabricMetadataKind::Subscriptions,
        101,
        17,
        b"orders.created\0orders.updated\0",
        1024,
    )
    .unwrap()
    .remove(0);

    let encoded = encode_fab1_chunk(&chunk, 2048).unwrap();
    assert_eq!(&encoded[..4], b"FAB1");

    let (decoded, consumed) = decode_fab1_chunk(&encoded, 2048, 64).unwrap();
    assert_eq!(decoded, chunk);
    assert_eq!(consumed, encoded.len());
}

#[test]
fn fab1_decoder_reports_consumed_bytes_and_leaves_later_extensions_untouched() {
    let chunk = chunk_snapshot(
        FabricMetadataKind::Services,
        202,
        3,
        b"service-directory-payload",
        1024,
    )
    .unwrap()
    .remove(0);
    let mut encoded = encode_fab1_chunk(&chunk, 2048).unwrap();
    let fab1_len = encoded.len();
    encoded.extend_from_slice(b"NEXTextension");

    let (decoded, consumed) = decode_fab1_chunk(&encoded, 2048, 64).unwrap();
    assert_eq!(decoded, chunk);
    assert_eq!(consumed, fab1_len);
    assert_eq!(&encoded[consumed..], b"NEXTextension");
}

#[test]
fn fab1_rejects_truncated_header_and_payload() {
    let chunk = chunk_snapshot(
        FabricMetadataKind::Load,
        303,
        8,
        &[7; 300],
        1024,
    )
    .unwrap()
    .remove(0);
    let encoded = encode_fab1_chunk(&chunk, 1024).unwrap();

    for cut in [0, 1, 4, 16, 64, encoded.len() - 1] {
        assert_eq!(
            decode_fab1_chunk(&encoded[..cut], 1024, 64),
            Err(FabricMetadataWireError::Truncated)
        );
    }
}

#[test]
fn fab1_rejects_unknown_kind_and_invalid_chunk_coordinates() {
    let chunk = chunk_snapshot(
        FabricMetadataKind::Subscriptions,
        404,
        1,
        b"payload",
        1024,
    )
    .unwrap()
    .remove(0);
    let encoded = encode_fab1_chunk(&chunk, 1024).unwrap();

    let mut bad_kind = encoded.clone();
    bad_kind[4] = 99;
    assert_eq!(
        decode_fab1_chunk(&bad_kind, 1024, 64),
        Err(FabricMetadataWireError::UnknownKind)
    );

    let mut bad_index = encoded.clone();
    // Header offsets: magic 0..4, kind 4, owner 5..13, generation 13..21,
    // hash 21..53, index 53..57, count 57..61, payload-len 61..65.
    bad_index[53..57].copy_from_slice(&1u32.to_be_bytes());
    assert_eq!(
        decode_fab1_chunk(&bad_index, 1024, 64),
        Err(FabricMetadataWireError::InvalidChunk)
    );
}

#[test]
fn fab1_enforces_payload_and_chunk_count_bounds_before_copying_payload() {
    let chunks = chunk_snapshot(
        FabricMetadataKind::Subscriptions,
        505,
        2,
        &[9; 1500],
        500,
    )
    .unwrap();
    let encoded = encode_fab1_chunk(&chunks[0], 500).unwrap();

    assert_eq!(
        decode_fab1_chunk(&encoded, 499, 64),
        Err(FabricMetadataWireError::PayloadTooLarge)
    );
    assert_eq!(
        decode_fab1_chunk(&encoded, 500, 2),
        Err(FabricMetadataWireError::TooManyChunks)
    );
}

#[test]
fn fab1_encoder_rejects_invalid_generation_and_oversized_payload() {
    let mut chunk = chunk_snapshot(
        FabricMetadataKind::Services,
        606,
        7,
        &[1; 32],
        1024,
    )
    .unwrap()
    .remove(0);

    assert_eq!(
        encode_fab1_chunk(&chunk, 31),
        Err(FabricMetadataWireError::PayloadTooLarge)
    );

    chunk.generation = 0;
    assert_eq!(
        encode_fab1_chunk(&chunk, 1024),
        Err(FabricMetadataWireError::InvalidChunk)
    );
}
