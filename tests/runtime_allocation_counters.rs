//! Counter contracts for the existing actor heap and opt-in iso arena.
//! Counters reflect successful allocator requests, not OS allocations or RSS.
use nulang::iso_arena::IsoArena;
use nulang::runtime::heap::{ActorHeap, TypeTag};
use nulang::runtime::Actor;

#[test]
fn heap_counts_bump_los_and_free_list_reuse_without_double_counting_frees() {
    let mut heap = ActorHeap::new(16 * 1024);
    let initial = heap.allocation_snapshot();
    assert_eq!(initial.allocations, 0);
    assert_eq!(initial.frees, 0);
    assert_eq!(initial.requested_payload_bytes, 0);
    assert_eq!(initial.live_objects, 0);

    let small = heap.alloc(17, TypeTag::Raw).expect("bump alloc");
    let large = heap.alloc(1024, TypeTag::Array).expect("LOS alloc");
    let before_free = heap.allocation_snapshot();
    assert_eq!(before_free.allocations, 2);
    assert_eq!(before_free.requested_payload_bytes, 17 + 1024);
    assert_eq!(before_free.live_objects, 2);

    unsafe {
        heap.free(small);
        heap.free(large);
    }
    let freed = heap.allocation_snapshot();
    assert_eq!(freed.allocations, 2, "free must not increment allocations");
    assert_eq!(freed.frees, 2);
    assert_eq!(freed.live_objects, 0);
    assert_eq!(freed.requested_payload_bytes, 17 + 1024);

    let reused_small = heap.alloc(17, TypeTag::Raw).expect("small reuse");
    let reused_large = heap.alloc(1024, TypeTag::Array).expect("LOS reuse");
    assert_eq!(reused_small, small);
    assert_eq!(reused_large, large);
    let reused = heap.allocation_snapshot();
    assert_eq!(reused.allocations, 4);
    assert_eq!(reused.frees, 2);
    assert_eq!(reused.live_objects, 2);
    assert_eq!(reused.requested_payload_bytes, (17 + 1024) * 2);
}

#[test]
fn heap_snapshot_resets_with_actor_heap_reuse() {
    let mut heap = ActorHeap::new(16 * 1024);
    heap.alloc(9, TypeTag::Raw).expect("allocation");
    assert_eq!(heap.allocation_snapshot().requested_payload_bytes, 9);
    heap.reset();
    let stats = heap.allocation_snapshot();
    assert_eq!(stats.allocations, 0);
    assert_eq!(stats.frees, 0);
    assert_eq!(stats.live_objects, 0);
    assert_eq!(stats.requested_payload_bytes, 0);

    heap.alloc(12, TypeTag::Raw).expect("allocation after reset");
    assert_eq!(heap.allocation_snapshot().allocations, 1);
    assert_eq!(heap.allocation_snapshot().requested_payload_bytes, 12);
}

#[test]
fn arena_counts_successful_payload_bytes_across_epoch_resets() {
    let mut arena = IsoArena::new();
    let initial = arena.allocation_snapshot();
    assert_eq!(initial.allocations, 0);
    assert_eq!(initial.epoch_allocations, 0);
    assert_eq!(initial.requested_payload_bytes, 0);
    assert_eq!(initial.resets, 0);

    arena.alloc(17, TypeTag::Raw).expect("first arena allocation");
    let allocated = arena.allocation_snapshot();
    assert_eq!(allocated.allocations, 1);
    assert_eq!(allocated.epoch_allocations, 1);
    assert_eq!(allocated.requested_payload_bytes, 17);

    // A rejected size must not increment successful-allocation counters.
    assert!(arena.alloc(usize::MAX, TypeTag::Raw).is_none());
    assert_eq!(arena.allocation_snapshot(), allocated);

    arena.reset();
    let reset = arena.allocation_snapshot();
    assert_eq!(reset.allocations, 1, "lifetime counter survives epoch reset");
    assert_eq!(reset.epoch_allocations, 0);
    assert_eq!(reset.requested_payload_bytes, 17);
    assert_eq!(reset.resets, 1);

    arena.alloc(33, TypeTag::Array).expect("second epoch allocation");
    let after = arena.allocation_snapshot();
    assert_eq!(after.allocations, 2);
    assert_eq!(after.epoch_allocations, 1);
    assert_eq!(after.requested_payload_bytes, 50);
    assert_eq!(after.resets, 1);
}

#[test]
fn allocator_snapshot_json_exposes_counters_not_eligibility_guesses() {
    let mut heap = ActorHeap::new(16 * 1024);
    heap.alloc(13, TypeTag::Raw).expect("heap allocation");
    let json = serde_json::to_value(heap.allocation_snapshot()).expect("JSON");
    assert_eq!(json["allocations"], 1);
    assert_eq!(json["requested_payload_bytes"], 13);
    assert_eq!(json["live_objects"], 1);
    assert!(json.get("arena_eligible").is_none());

    let mut arena = IsoArena::new();
    arena.alloc(5, TypeTag::Raw).expect("arena allocation");
    let json = serde_json::to_value(arena.allocation_snapshot()).expect("JSON");
    assert_eq!(json["allocations"], 1);
    assert_eq!(json["epoch_allocations"], 1);
    assert_eq!(json["requested_payload_bytes"], 5);
}

#[test]
fn actor_snapshot_keeps_heap_and_arena_lifetimes_separate() {
    let mut actor = Actor::new(42, "metrics", 16);
    actor.heap.alloc(11, TypeTag::Raw).expect("actor heap alloc");
    actor.iso_arena.alloc(7, TypeTag::Raw).expect("actor arena alloc");

    let first = actor.allocation_snapshot();
    assert_eq!(first.actor_id, 42);
    assert_eq!(first.heap.allocations, 1);
    assert_eq!(first.heap.requested_payload_bytes, 11);
    assert_eq!(first.arena.allocations, 1);
    assert_eq!(first.arena.requested_payload_bytes, 7);

    // Actor pooling resets heap counters, but an arena epoch reset preserves
    // its lifetime counters. The snapshot must never combine those scopes.
    actor.reset(43, "reused", 16);
    let recycled = actor.allocation_snapshot();
    assert_eq!(recycled.actor_id, 43);
    assert_eq!(recycled.heap.allocations, 0);
    assert_eq!(recycled.heap.requested_payload_bytes, 0);
    assert_eq!(recycled.arena.allocations, 1);
    assert_eq!(recycled.arena.epoch_allocations, 0);
    assert!(recycled.arena.resets >= 1);
}
