//! Allocation reporting contract for the existing runtime metrics pipeline.
//! Snapshot totals are point-in-time gauges, not monotonic process counters.
use nulang::runtime::heap::TypeTag;
use nulang::runtime::{Actor, Runtime};

#[test]
fn aggregates_main_heap_and_live_actors_without_including_pooled_actors() {
    let mut rt = Runtime::new();
    let initial = rt.metrics_snapshot();
    assert_eq!(initial.allocator.heap_allocations_since_reset, 0);
    assert_eq!(initial.allocator.heap_requested_payload_bytes_since_reset, 0);
    assert_eq!(initial.allocator.heap_live_objects, 0);
    assert_eq!(initial.allocator.arena_epoch_allocations, 0);

    rt.main_heap.alloc(11, TypeTag::Raw).expect("main heap allocation");

    let mut actor = Actor::new(42, "active", 16);
    actor.heap.alloc(17, TypeTag::Array).expect("actor heap allocation");
    actor.iso_arena.alloc(23, TypeTag::Raw).expect("actor arena allocation");
    rt.actors.insert(42, actor);

    let mut pooled = Actor::new(99, "pooled", 16);
    pooled.heap.alloc(101, TypeTag::Raw).expect("pooled heap allocation");
    pooled.iso_arena.alloc(102, TypeTag::Raw).expect("pooled arena allocation");
    rt.actor_pool.push(pooled);

    let metric = rt.metrics_snapshot().allocator;
    assert_eq!(metric.heap_allocations_since_reset, 2);
    assert_eq!(metric.heap_requested_payload_bytes_since_reset, 28);
    assert_eq!(metric.heap_live_objects, 2);
    assert_eq!(metric.arena_epoch_allocations, 1);
}

#[test]
fn exported_allocator_metrics_are_gauges_and_allow_actor_churn() {
    let mut rt = Runtime::new();
    let mut actor = Actor::new(42, "active", 16);
    actor.heap.alloc(9, TypeTag::Raw).expect("heap allocation");
    actor.iso_arena.alloc(10, TypeTag::Raw).expect("arena allocation");
    rt.actors.insert(42, actor);

    let first = rt.metrics_snapshot();
    let json = serde_json::to_value(&first).expect("serialize metrics");
    assert_eq!(json["allocator"]["heap_allocations_since_reset"], 1);
    assert_eq!(json["allocator"]["heap_requested_payload_bytes_since_reset"], 9);
    assert_eq!(json["allocator"]["arena_epoch_allocations"], 1);

    let prometheus = first.to_prometheus_text();
    for name in [
        "nulang_allocator_heap_allocations_since_reset",
        "nulang_allocator_heap_requested_payload_bytes_since_reset",
        "nulang_allocator_heap_live_objects",
        "nulang_allocator_arena_epoch_allocations",
    ] {
        assert!(
            prometheus.contains(&format!("# TYPE {name} gauge\n")),
            "{name} must be a gauge, not a counter"
        );
    }

    // A pooled actor should contribute nothing to the active-actor gauges.
    rt.actor_pool.push(rt.actors.remove(&42).expect("active actor"));
    let gone = rt.metrics_snapshot().allocator;
    assert_eq!(gone.heap_allocations_since_reset, 0);
    assert_eq!(gone.heap_requested_payload_bytes_since_reset, 0);
    assert_eq!(gone.heap_live_objects, 0);
    assert_eq!(gone.arena_epoch_allocations, 0);
}

#[test]
fn arena_epoch_resets_are_reflected_without_counting_lifetime_allocations() {
    let mut rt = Runtime::new();
    let mut actor = Actor::new(4, "epoch", 16);
    actor.iso_arena.alloc(8, TypeTag::Raw).expect("first epoch");
    actor.iso_arena.reset();
    actor.iso_arena.alloc(5, TypeTag::Raw).expect("second epoch");
    rt.actors.insert(4, actor);

    let metric = rt.metrics_snapshot().allocator;
    assert_eq!(metric.arena_epoch_allocations, 1);
    assert_eq!(metric.heap_allocations_since_reset, 0);
}
