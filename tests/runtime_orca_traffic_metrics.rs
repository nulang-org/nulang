//! Existing ORCA operation counters must be exposed as active-GC gauges.
//! These counters are not guaranteed to cover direct header mutations.
use nulang::runtime::heap::TypeTag;
use nulang::runtime::{Actor, Runtime};

#[test]
fn orca_snapshot_includes_main_and_active_gcs_but_not_pooled_actors() {
    let mut runtime = Runtime::new();
    let main_ptr = runtime.main_gc
        .alloc_object(&mut runtime.main_heap, 8, TypeTag::Raw)
        .expect("main GC allocation");
    unsafe {
        runtime.main_gc.local_ref(&runtime.main_heap, main_ptr);
        runtime.main_gc.drop_local_ref(&mut runtime.main_heap, main_ptr);
    }

    let mut actor = Actor::new(20, "live", 8);
    let actor_ptr = actor.orca_gc
        .alloc_object(&mut actor.heap, 16, TypeTag::Raw)
        .expect("actor GC allocation");
    unsafe {
        actor.orca_gc.local_ref(&actor.heap, actor_ptr);
        actor.orca_gc.local_ref(&actor.heap, actor_ptr);
        actor.orca_gc.drop_local_ref(&mut actor.heap, actor_ptr);
    }
    runtime.actors.insert(20, actor);

    let mut pooled = Actor::new(30, "pooled", 8);
    let pooled_ptr = pooled.orca_gc
        .alloc_object(&mut pooled.heap, 8, TypeTag::Raw)
        .expect("pooled allocation");
    unsafe { pooled.orca_gc.local_ref(&pooled.heap, pooled_ptr); }
    runtime.actor_pool.push(pooled);

    let snap = runtime.metrics_snapshot();
    assert_eq!(snap.orca_traffic.local_refs_created, 3);
    assert_eq!(snap.orca_traffic.local_refs_dropped, 2);

    runtime.actor_pool.push(runtime.actors.remove(&20).unwrap());
    let after = runtime.metrics_snapshot().orca_traffic;
    assert_eq!(after.local_refs_created, 1, "main GC remains active");
    assert_eq!(after.local_refs_dropped, 1);
}

#[test]
fn orca_traffic_exposes_nonmonotonic_gauges_in_json_and_prometheus() {
    let mut runtime = Runtime::new();
    let mut actor = Actor::new(20, "live", 8);
    let ptr = actor.orca_gc
        .alloc_object(&mut actor.heap, 8, TypeTag::Raw)
        .expect("actor allocation");
    unsafe { actor.orca_gc.local_ref(&actor.heap, ptr); }
    runtime.actors.insert(20, actor);

    let snapshot = runtime.metrics_snapshot();
    let json = serde_json::to_value(&snapshot).expect("serialize snapshot");
    assert_eq!(json["orca_traffic"]["local_refs_created"], 1);
    assert_eq!(json["orca_traffic"]["local_refs_dropped"], 0);

    let metrics = snapshot.to_prometheus_text();
    for name in [
        "nulang_orca_local_refs_created_active",
        "nulang_orca_local_refs_dropped_active",
        "nulang_orca_foreign_refs_sent_active",
        "nulang_orca_foreign_refs_received_active",
    ] {
        assert!(
            metrics.contains(&format!("# TYPE {name} gauge\n")),
            "{name} must be a gauge because actors exit and reset"
        );
    }
    runtime.actors.remove(&20);
    assert_eq!(runtime.metrics_snapshot().orca_traffic.local_refs_created, 0);
}

#[test]
fn orca_ref_traffic_snapshot_is_read_only() {
    let mut runtime = Runtime::new();
    let mut actor = Actor::new(4, "read-only", 8);
    let ptr = actor.orca_gc
        .alloc_object(&mut actor.heap, 8, TypeTag::Raw)
        .expect("actor allocation");
    unsafe { actor.orca_gc.local_ref(&actor.heap, ptr); }
    runtime.actors.insert(4, actor);

    let first = runtime.metrics_snapshot().orca_traffic;
    let second = runtime.metrics_snapshot().orca_traffic;
    assert_eq!(first.local_refs_created, second.local_refs_created);
    assert_eq!(first.local_refs_dropped, second.local_refs_dropped);
    assert_eq!(first.foreign_refs_sent, second.foreign_refs_sent);
    assert_eq!(first.foreign_refs_received, second.foreign_refs_received);
}

#[test]
fn existing_orca_foreign_reference_operations_are_visible() {
    let mut runtime = Runtime::new();
    let mut actor = Actor::new(12, "sender", 8);
    let ptr = actor.orca_gc
        .alloc_object(&mut actor.heap, 8, TypeTag::Raw)
        .expect("allocated ORCA object");
    // Exercise the existing in-flight and returned-to-owner protocol without
    // changing GC behavior or injecting fabricated metrics.
    unsafe {
        let _in_flight = actor.orca_gc.send_ref_to(&actor.heap, ptr, 99);
        actor.orca_gc.receive_ref(&actor.heap, ptr);
    }
    runtime.actors.insert(12, actor);

    let stats = runtime.metrics_snapshot().orca_traffic;
    assert_eq!(stats.foreign_refs_sent, 1);
    assert_eq!(stats.foreign_refs_received, 1);
}
