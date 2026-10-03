#[path = "../src/runtime/fabric_metadata.rs"]
mod fabric_metadata;

use fabric_metadata::{
    chunk_snapshot, FabricMetadataAssembler, FabricMetadataChunk, FabricMetadataKind,
    MetadataAssemblyError,
};

fn payload(len: usize) -> Vec<u8> {
    (0..len).map(|index| (index % 251) as u8).collect()
}

#[test]
fn chunk_snapshot_splits_large_metadata_without_truncating() {
    let bytes = payload(32 * 1024 + 17);
    let chunks = chunk_snapshot(
        FabricMetadataKind::Subscriptions,
        7,
        42,
        &bytes,
        1024,
    )
    .unwrap();

    assert!(chunks.len() > 32);
    assert_eq!(chunks[0].chunk_index, 0);
    assert_eq!(chunks[0].chunk_count as usize, chunks.len());
    assert!(chunks.iter().all(|chunk| chunk.payload.len() <= 1024));
    assert!(chunks.iter().all(|chunk| chunk.owner == 7));
    assert!(chunks.iter().all(|chunk| chunk.generation == 42));
    assert!(chunks
        .iter()
        .all(|chunk| chunk.snapshot_hash == chunks[0].snapshot_hash));

    let reassembled: Vec<u8> = chunks
        .iter()
        .flat_map(|chunk| chunk.payload.iter().copied())
        .collect();
    assert_eq!(reassembled, bytes);
}

#[test]
fn out_of_order_chunks_publish_only_after_complete_snapshot() {
    let bytes = payload(4097);
    let mut chunks = chunk_snapshot(
        FabricMetadataKind::Subscriptions,
        11,
        9,
        &bytes,
        1024,
    )
    .unwrap();
    assert_eq!(chunks.len(), 5);

    let mut assembler = FabricMetadataAssembler::new(64 * 1024, 128);

    assert!(assembler.push(chunks.remove(3)).unwrap().is_none());
    assert!(assembler.push(chunks.remove(1)).unwrap().is_none());
    assert!(assembler.push(chunks.remove(2)).unwrap().is_none());
    assert!(assembler.push(chunks.remove(0)).unwrap().is_none());

    let completed = assembler.push(chunks.remove(0)).unwrap().unwrap();
    assert_eq!(completed.kind, FabricMetadataKind::Subscriptions);
    assert_eq!(completed.owner, 11);
    assert_eq!(completed.generation, 9);
    assert_eq!(completed.payload, bytes);
    assert_eq!(assembler.latest_generation(FabricMetadataKind::Subscriptions, 11), Some(9));
}

#[test]
fn identical_duplicate_chunk_is_idempotent() {
    let chunks = chunk_snapshot(
        FabricMetadataKind::Subscriptions,
        22,
        3,
        &payload(2500),
        1000,
    )
    .unwrap();
    let mut assembler = FabricMetadataAssembler::new(16 * 1024, 16);

    assert!(assembler.push(chunks[0].clone()).unwrap().is_none());
    assert!(assembler.push(chunks[0].clone()).unwrap().is_none());
    assert!(assembler.push(chunks[2].clone()).unwrap().is_none());
    let completed = assembler.push(chunks[1].clone()).unwrap().unwrap();

    assert_eq!(completed.generation, 3);
    assert_eq!(completed.payload, payload(2500));
}

#[test]
fn conflicting_duplicate_chunk_fails_closed() {
    let chunks = chunk_snapshot(
        FabricMetadataKind::Subscriptions,
        33,
        5,
        &payload(2200),
        1000,
    )
    .unwrap();
    let mut assembler = FabricMetadataAssembler::new(16 * 1024, 16);

    assembler.push(chunks[0].clone()).unwrap();
    let mut conflict = chunks[0].clone();
    conflict.payload[0] ^= 0xff;

    assert_eq!(
        assembler.push(conflict),
        Err(MetadataAssemblyError::ConflictingChunk)
    );
    assert_eq!(assembler.inflight_snapshot_count(), 0);
}

#[test]
fn newer_generation_supersedes_incomplete_older_generation() {
    let old = chunk_snapshot(
        FabricMetadataKind::Subscriptions,
        44,
        10,
        &payload(2400),
        1000,
    )
    .unwrap();
    let fresh_bytes = payload(3100);
    let fresh = chunk_snapshot(
        FabricMetadataKind::Subscriptions,
        44,
        11,
        &fresh_bytes,
        1000,
    )
    .unwrap();
    let mut assembler = FabricMetadataAssembler::new(16 * 1024, 16);

    assert!(assembler.push(old[0].clone()).unwrap().is_none());
    assert!(assembler.push(fresh[1].clone()).unwrap().is_none());

    assert_eq!(
        assembler.push(old[1].clone()),
        Err(MetadataAssemblyError::StaleGeneration)
    );

    for chunk in fresh.iter().filter(|chunk| chunk.chunk_index != 1) {
        let result = assembler.push(chunk.clone()).unwrap();
        if chunk.chunk_index + 1 == chunk.chunk_count {
            assert_eq!(result.unwrap().payload, fresh_bytes);
        }
    }
    assert_eq!(assembler.latest_generation(FabricMetadataKind::Subscriptions, 44), Some(11));
}

#[test]
fn corrupted_snapshot_hash_never_becomes_visible() {
    let mut chunks = chunk_snapshot(
        FabricMetadataKind::Services,
        55,
        2,
        &payload(2500),
        1000,
    )
    .unwrap();
    let mut assembler = FabricMetadataAssembler::new(16 * 1024, 16);

    let last = chunks.len() - 1;
    chunks[last].payload[0] ^= 0x7f;

    for chunk in chunks.iter().take(last) {
        assert!(assembler.push(chunk.clone()).unwrap().is_none());
    }
    assert_eq!(
        assembler.push(chunks[last].clone()),
        Err(MetadataAssemblyError::SnapshotHashMismatch)
    );
    assert_eq!(assembler.latest_generation(FabricMetadataKind::Services, 55), None);
    assert_eq!(assembler.inflight_snapshot_count(), 0);
}

#[test]
fn invalid_or_oversized_chunks_are_rejected_before_allocation_growth() {
    let mut assembler = FabricMetadataAssembler::new(1500, 4);
    let bytes = payload(1800);
    let chunks = chunk_snapshot(FabricMetadataKind::Load, 66, 1, &bytes, 900).unwrap();

    assert!(assembler.push(chunks[0].clone()).unwrap().is_none());
    assert_eq!(
        assembler.push(chunks[1].clone()),
        Err(MetadataAssemblyError::SnapshotTooLarge)
    );
    assert_eq!(assembler.inflight_snapshot_count(), 0);

    let invalid = FabricMetadataChunk {
        owner: 66,
        kind: FabricMetadataKind::Load,
        generation: 2,
        snapshot_hash: [0; 32],
        chunk_index: 4,
        chunk_count: 4,
        payload: vec![],
    };
    assert_eq!(
        assembler.push(invalid),
        Err(MetadataAssemblyError::InvalidChunk)
    );
}

#[test]
fn empty_snapshot_is_one_atomic_chunk() {
    let chunks = chunk_snapshot(FabricMetadataKind::Services, 77, 8, &[], 1024).unwrap();
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].chunk_index, 0);
    assert_eq!(chunks[0].chunk_count, 1);

    let mut assembler = FabricMetadataAssembler::new(1024, 4);
    let completed = assembler.push(chunks[0].clone()).unwrap().unwrap();
    assert!(completed.payload.is_empty());
    assert_eq!(completed.generation, 8);
}
