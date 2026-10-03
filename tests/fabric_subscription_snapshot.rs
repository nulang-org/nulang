#[path = "../src/runtime/fabric_metadata.rs"]
mod fabric_metadata;
#[path = "../src/runtime/fabric_subscription_metadata.rs"]
mod fabric_subscription_metadata;
#[path = "../src/runtime/fabric_subscription_snapshot.rs"]
mod fabric_subscription_snapshot;

use fabric_metadata::{FabricMetadataKind, MetadataAssemblyError};
use fabric_subscription_metadata::FabricSubscriptionMetadataEntry;
use fabric_subscription_snapshot::{
    FabricSubscriptionSnapshotAssembler, FabricSubscriptionSnapshotCodec,
    FabricSubscriptionSnapshotError,
};

fn entry(index: usize) -> FabricSubscriptionMetadataEntry {
    FabricSubscriptionMetadataEntry {
        pattern: format!("tenant.{index}.events.*"),
        actor_id: index as u64 + 1,
        behavior: "handle".to_string(),
        group: (index % 3 == 0).then(|| format!("workers-{}", index % 11)),
    }
}

fn codec() -> FabricSubscriptionSnapshotCodec {
    FabricSubscriptionSnapshotCodec::new(10_000, 4 * 1024 * 1024, 4096, 16 * 1024)
}

fn assembler() -> FabricSubscriptionSnapshotAssembler {
    FabricSubscriptionSnapshotAssembler::new(
        10_000,
        4 * 1024 * 1024,
        4096,
        512,
        16 * 1024 * 1024,
    )
}

#[test]
fn more_than_fab0_limit_roundtrips_through_fab1_chunks() {
    let subscriptions: Vec<_> = (0..5000).map(entry).collect();
    let chunks = codec().encode_chunks(17, 3, &subscriptions).unwrap();
    assert!(chunks.len() > 1);
    assert!(chunks.iter().all(|chunk| {
        chunk.owner == 17
            && chunk.generation == 3
            && chunk.kind == FabricMetadataKind::Subscriptions
    }));

    let mut receiver = assembler();
    let mut completed = None;
    for chunk in chunks.into_iter().rev() {
        let next = receiver.push(chunk).unwrap();
        if next.is_some() {
            assert!(
                completed.is_none(),
                "one generation must publish exactly once"
            );
            completed = next;
        }
    }

    let completed = completed.expect("complete generation must publish");
    assert_eq!(completed.owner, 17);
    assert_eq!(completed.generation, 3);
    assert_eq!(completed.subscriptions, subscriptions);
}

#[test]
fn incomplete_generation_never_exposes_partial_subscriptions() {
    let subscriptions: Vec<_> = (0..5000).map(entry).collect();
    let chunks = codec().encode_chunks(23, 7, &subscriptions).unwrap();
    assert!(chunks.len() > 2);

    let mut receiver = assembler();
    for chunk in chunks.iter().take(chunks.len() - 1).cloned() {
        assert_eq!(receiver.push(chunk).unwrap(), None);
    }

    let complete = receiver
        .push(chunks.last().unwrap().clone())
        .unwrap()
        .unwrap();
    assert_eq!(complete.subscriptions, subscriptions);
}

#[test]
fn corrupted_generation_fails_closed_without_partial_output() {
    let subscriptions: Vec<_> = (0..5000).map(entry).collect();
    let mut chunks = codec().encode_chunks(31, 11, &subscriptions).unwrap();
    let last = chunks.last_mut().unwrap();
    last.payload[0] ^= 0xFF;

    let mut receiver = assembler();
    for chunk in chunks.iter().take(chunks.len() - 1).cloned() {
        assert_eq!(receiver.push(chunk).unwrap(), None);
    }
    assert_eq!(
        receiver.push(chunks.last().unwrap().clone()),
        Err(FabricSubscriptionSnapshotError::Metadata(
            MetadataAssemblyError::SnapshotHashMismatch
        ))
    );
}

#[test]
fn newer_generation_supersedes_incomplete_older_generation_atomically() {
    let old: Vec<_> = (0..5000).map(entry).collect();
    let new: Vec<_> = (6000..6100).map(entry).collect();
    let old_chunks = codec().encode_chunks(41, 20, &old).unwrap();
    let new_chunks = codec().encode_chunks(41, 21, &new).unwrap();

    let mut receiver = assembler();
    assert_eq!(receiver.push(old_chunks[0].clone()).unwrap(), None);

    let mut completed = None;
    for chunk in new_chunks {
        completed = receiver.push(chunk).unwrap().or(completed);
    }
    let completed = completed.expect("newer complete generation must publish");
    assert_eq!(completed.generation, 21);
    assert_eq!(completed.subscriptions, new);

    assert_eq!(
        receiver.push(old_chunks[1].clone()),
        Err(FabricSubscriptionSnapshotError::Metadata(
            MetadataAssemblyError::StaleGeneration
        ))
    );
}

#[test]
fn wrong_metadata_kind_is_rejected_before_assembly() {
    let subscriptions = vec![entry(0)];
    let mut chunk = codec()
        .encode_chunks(51, 2, &subscriptions)
        .unwrap()
        .remove(0);
    chunk.kind = FabricMetadataKind::Services;

    let mut receiver = assembler();
    assert_eq!(
        receiver.push(chunk),
        Err(FabricSubscriptionSnapshotError::WrongMetadataKind)
    );
    assert_eq!(receiver.inflight_snapshot_count(), 0);
}

#[test]
fn empty_authoritative_snapshot_roundtrips() {
    let chunks = codec().encode_chunks(61, 5, &[]).unwrap();
    let mut receiver = assembler();
    let completed = receiver.push(chunks[0].clone()).unwrap().unwrap();
    assert!(completed.subscriptions.is_empty());
}

#[test]
fn discard_owner_clears_incomplete_and_generation_state() {
    let subscriptions: Vec<_> = (0..5000).map(entry).collect();
    let chunks = codec().encode_chunks(71, 9, &subscriptions).unwrap();
    let mut receiver = assembler();
    assert_eq!(receiver.push(chunks[0].clone()).unwrap(), None);
    assert_eq!(receiver.inflight_snapshot_count(), 1);

    receiver.discard_owner(71);
    assert_eq!(receiver.inflight_snapshot_count(), 0);

    // A restarted node with the same stable owner may begin from generation 1.
    let restarted = codec().encode_chunks(71, 1, &[entry(9999)]).unwrap();
    let completed = receiver.push(restarted[0].clone()).unwrap().unwrap();
    assert_eq!(completed.generation, 1);
}
