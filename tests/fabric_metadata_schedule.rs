#[path = "../src/runtime/fabric_metadata.rs"]
mod fabric_metadata;

use fabric_metadata::{
    chunk_snapshot, FabricMetadataChunkCursor, FabricMetadataKind, MetadataScheduleError,
};

fn chunks(owner: u64, generation: u64, len: usize) -> Vec<fabric_metadata::FabricMetadataChunk> {
    chunk_snapshot(
        FabricMetadataKind::Subscriptions,
        owner,
        generation,
        &vec![7; len],
        100,
    )
    .unwrap()
}

#[test]
fn cursor_rotates_every_chunk_and_wraps() {
    let snapshot = chunks(1, 10, 250);
    assert_eq!(snapshot.len(), 3);
    let mut cursor = FabricMetadataChunkCursor::default();

    let indices: Vec<u32> = (0..7)
        .map(|_| cursor.next(&snapshot).unwrap().chunk_index)
        .collect();

    assert_eq!(indices, vec![0, 1, 2, 0, 1, 2, 0]);
}

#[test]
fn newer_generation_resets_rotation_to_chunk_zero() {
    let old = chunks(2, 5, 250);
    let fresh = chunks(2, 6, 250);
    let mut cursor = FabricMetadataChunkCursor::default();

    assert_eq!(cursor.next(&old).unwrap().chunk_index, 0);
    assert_eq!(cursor.next(&old).unwrap().chunk_index, 1);
    assert_eq!(cursor.next(&fresh).unwrap().chunk_index, 0);
    assert_eq!(cursor.next(&fresh).unwrap().chunk_index, 1);
}

#[test]
fn separate_metadata_namespaces_rotate_independently() {
    let subscriptions = chunks(3, 7, 250);
    let services = chunk_snapshot(
        FabricMetadataKind::Services,
        3,
        7,
        &vec![9; 250],
        100,
    )
    .unwrap();
    let mut cursor = FabricMetadataChunkCursor::default();

    assert_eq!(cursor.next(&subscriptions).unwrap().chunk_index, 0);
    assert_eq!(cursor.next(&subscriptions).unwrap().chunk_index, 1);
    assert_eq!(cursor.next(&services).unwrap().chunk_index, 0);
    assert_eq!(cursor.next(&subscriptions).unwrap().chunk_index, 2);
    assert_eq!(cursor.next(&services).unwrap().chunk_index, 1);
}

#[test]
fn cursor_rejects_incomplete_or_mixed_snapshot_vectors() {
    let snapshot = chunks(4, 2, 250);
    let mut cursor = FabricMetadataChunkCursor::default();

    assert_eq!(
        cursor.next(&snapshot[..2]),
        Err(MetadataScheduleError::IncompleteSnapshot)
    );

    let mut mixed = snapshot.clone();
    mixed[1].generation = 3;
    assert_eq!(
        cursor.next(&mixed),
        Err(MetadataScheduleError::MixedSnapshot)
    );
}

#[test]
fn same_generation_with_different_hash_fails_closed() {
    let first = chunks(5, 9, 250);
    let mut conflicting = first.clone();
    for chunk in &mut conflicting {
        chunk.snapshot_hash[0] ^= 0xff;
    }
    let mut cursor = FabricMetadataChunkCursor::default();

    assert_eq!(cursor.next(&first).unwrap().chunk_index, 0);
    assert_eq!(
        cursor.next(&conflicting),
        Err(MetadataScheduleError::ConflictingGeneration)
    );
}

#[test]
fn cursor_accepts_chunks_in_any_vector_order_but_sends_by_chunk_index() {
    let mut snapshot = chunks(6, 3, 250);
    snapshot.swap(0, 2);
    let mut cursor = FabricMetadataChunkCursor::default();

    assert_eq!(cursor.next(&snapshot).unwrap().chunk_index, 0);
    assert_eq!(cursor.next(&snapshot).unwrap().chunk_index, 1);
    assert_eq!(cursor.next(&snapshot).unwrap().chunk_index, 2);
}
