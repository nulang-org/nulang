#[path = "../src/runtime/fabric_metadata.rs"]
mod fabric_metadata;
#[path = "../src/runtime/fabric_subscription_metadata.rs"]
mod fabric_subscription_metadata;
#[path = "../src/runtime/fabric_subscription_snapshot.rs"]
mod fabric_subscription_snapshot;
#[path = "../src/runtime/fabric_subscription_gossip.rs"]
mod fabric_subscription_gossip;

use fabric_subscription_gossip::{
    FabricSubscriptionGossipError, FabricSubscriptionGossipSender,
};
use fabric_subscription_metadata::FabricSubscriptionMetadataEntry;
use fabric_subscription_snapshot::FabricSubscriptionSnapshotCodec;

fn entries(count: usize) -> Vec<FabricSubscriptionMetadataEntry> {
    (0..count)
        .map(|index| FabricSubscriptionMetadataEntry {
            pattern: format!("tenant.{index}.events.*"),
            actor_id: index as u64 + 1,
            behavior: "handle".to_string(),
            group: None,
        })
        .collect()
}

fn sender() -> FabricSubscriptionGossipSender {
    FabricSubscriptionGossipSender::new(FabricSubscriptionSnapshotCodec::new(
        10_000,
        4 * 1024 * 1024,
        4096,
        1024,
    ))
}

#[test]
fn same_generation_rotates_cached_chunks_without_reencoding() {
    let subscriptions = entries(500);
    let mut sender = sender();

    let first = sender.next_chunk(7, 3, &subscriptions).unwrap();
    assert!(first.chunk_count > 2);
    assert_eq!(first.chunk_index, 0);
    assert_eq!(sender.encode_count(), 1);

    let second = sender.next_chunk(7, 3, &subscriptions).unwrap();
    assert_eq!(second.chunk_index, 1);
    assert_eq!(sender.encode_count(), 1);

    for _ in 2..first.chunk_count {
        sender.next_chunk(7, 3, &subscriptions).unwrap();
    }
    let wrapped = sender.next_chunk(7, 3, &subscriptions).unwrap();
    assert_eq!(wrapped.chunk_index, 0);
    assert_eq!(sender.encode_count(), 1);
}

#[test]
fn newer_generation_reencodes_and_restarts_at_chunk_zero() {
    let subscriptions = entries(500);
    let mut sender = sender();

    assert_eq!(sender.next_chunk(11, 4, &subscriptions).unwrap().chunk_index, 0);
    assert_eq!(sender.next_chunk(11, 4, &subscriptions).unwrap().chunk_index, 1);

    let mut next = subscriptions;
    next.push(FabricSubscriptionMetadataEntry {
        pattern: "new.events.*".to_string(),
        actor_id: 999,
        behavior: "handle".to_string(),
        group: Some("workers".to_string()),
    });
    let chunk = sender.next_chunk(11, 5, &next).unwrap();
    assert_eq!(chunk.generation, 5);
    assert_eq!(chunk.chunk_index, 0);
    assert_eq!(sender.encode_count(), 2);
}

#[test]
fn stale_generation_is_rejected_without_reencoding() {
    let subscriptions = entries(500);
    let mut sender = sender();

    sender.next_chunk(13, 8, &subscriptions).unwrap();
    assert_eq!(
        sender.next_chunk(13, 7, &subscriptions),
        Err(FabricSubscriptionGossipError::StaleGeneration)
    );
    assert_eq!(sender.encode_count(), 1);
}

#[test]
fn generation_zero_is_rejected() {
    let subscriptions = entries(1);
    let mut sender = sender();
    assert_eq!(
        sender.next_chunk(17, 0, &subscriptions),
        Err(FabricSubscriptionGossipError::InvalidGeneration)
    );
    assert_eq!(sender.encode_count(), 0);
}

#[test]
fn discard_owner_drops_cache_and_cursor_state() {
    let subscriptions = entries(500);
    let mut sender = sender();

    sender.next_chunk(19, 9, &subscriptions).unwrap();
    sender.next_chunk(19, 9, &subscriptions).unwrap();
    assert_eq!(sender.cached_owner_count(), 1);

    sender.discard_owner(19);
    assert_eq!(sender.cached_owner_count(), 0);

    let restarted = sender.next_chunk(19, 1, &subscriptions).unwrap();
    assert_eq!(restarted.generation, 1);
    assert_eq!(restarted.chunk_index, 0);
    assert_eq!(sender.encode_count(), 2);
}
