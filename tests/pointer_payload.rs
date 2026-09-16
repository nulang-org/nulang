use nulang::runtime::heap::{ActorHeap, TypeTag};
use nulang::value_layout::ptr_fits_payload;

fn assert_fits(ptr: *mut u8, context: &str) {
    assert!(
        ptr_fits_payload(ptr as u64),
        "{context} allocation at {:#x} exceeds Nulang's 48-bit Value pointer payload",
        ptr as usize
    );
}

#[test]
fn actor_heap_allocations_fit_value_pointer_payload() {
    // Start deliberately small so repeated allocations exercise chained bump
    // blocks instead of checking only the initial allocator mapping.
    let mut heap = ActorHeap::new(1024);
    heap.set_actor_id(1);

    for _ in 0..512 {
        let ptr = heap
            .alloc(16, TypeTag::Array)
            .expect("small actor-heap allocation failed");
        assert_fits(ptr, "bump/chained-block");
    }

    // Large allocations use ActorHeap's large-object space and therefore
    // exercise a separate global-allocation path.
    for size in [512usize, 4096, 64 * 1024] {
        let ptr = heap
            .alloc(size, TypeTag::Raw)
            .expect("large-object-space allocation failed");
        assert_fits(ptr, "large-object-space");
    }
}

#[test]
fn representative_heap_pointer_round_trips_through_value() {
    let mut heap = ActorHeap::new(4096);
    let ptr = heap
        .alloc(32, TypeTag::Record)
        .expect("actor-heap allocation failed");
    assert_fits(ptr, "round-trip");

    let value = nulang::vm::Value::ptr(ptr);
    assert_eq!(value.as_ptr(), Some(ptr));
}
