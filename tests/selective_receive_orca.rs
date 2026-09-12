use std::cell::RefCell;
use std::rc::Rc;

use nulang::runtime::{
    heap::{ActorHeap, TypeTag},
    Runtime, RuntimeVmCallbacks,
};
use nulang::vm::{ActorVmCallbacks, Value};

fn pointer_message_runtime() -> (Rc<RefCell<Runtime>>, RuntimeVmCallbacks, *mut u8, u64) {
    let runtime = Rc::new(RefCell::new(Runtime::new()));
    let (receiver, ptr);

    {
        let mut rt = runtime.borrow_mut();
        let sender = rt.spawn_actor(Box::new(|| vec![]));
        receiver = rt.spawn_actor(Box::new(|| vec![]));

        ptr = {
            let actor = rt.actors.get_mut(&sender).expect("sender actor");
            actor
                .orca_gc
                .alloc_object(&mut actor.heap, 16, TypeTag::Raw)
                .expect("sender allocation")
        };

        rt.current_actor = Some(sender);
        rt.send_message_by_id(receiver, 7, &[Value::ptr(ptr)]);

        // Land the send-side in-flight ORCA operation before selective
        // receive starts. From this point on, foreign_count changes measure
        // receiver ownership holds rather than transport state.
        rt.process_gc_ops();
        rt.current_actor = Some(receiver);
    }

    let callbacks = RuntimeVmCallbacks::new(Rc::clone(&runtime));
    (runtime, callbacks, ptr, receiver)
}

fn foreign_count(ptr: *mut u8) -> u32 {
    // SAFETY: every test keeps the owning actor alive for the full assertion
    // sequence, and `ptr` is returned by that actor's live `ActorHeap`.
    unsafe { (*ActorHeap::header_of(ptr)).foreign_count }
}

#[test]
fn rejected_pointer_candidate_never_takes_receiver_hold() {
    let (runtime, mut callbacks, ptr, receiver) = pointer_message_runtime();
    assert_eq!(foreign_count(ptr), 0, "transport hold must already be landed");

    for _ in 0..4 {
        let candidate = callbacks
            .try_receive_match(&[7])
            .expect("pointer-bearing candidate");
        assert_eq!(candidate.1.len(), 1);
        assert_eq!(
            foreign_count(ptr),
            0,
            "candidate discovery must not establish receiver ownership"
        );
        assert_eq!(
            runtime.borrow().actors[&receiver].mailbox.len(),
            1,
            "rejected candidate must remain logically queued"
        );

        // A second scan models the VM moving past a failed pattern/guard.
        assert!(callbacks.try_receive_match(&[7]).is_none());
        assert_eq!(
            foreign_count(ptr),
            0,
            "repeated rejection must not accumulate ORCA holds"
        );
        callbacks.reset_receive_match();
        assert_eq!(foreign_count(ptr), 0, "reset must be ownership-neutral");
    }
}

#[test]
fn committed_pointer_candidate_takes_exactly_one_receiver_hold() {
    let (runtime, mut callbacks, ptr, receiver) = pointer_message_runtime();
    assert_eq!(foreign_count(ptr), 0, "transport hold must already be landed");

    callbacks
        .try_receive_match(&[7])
        .expect("pointer-bearing candidate");
    assert_eq!(
        foreign_count(ptr),
        0,
        "reservation alone must not establish receiver ownership"
    );

    callbacks.commit_receive_match();
    assert_eq!(
        foreign_count(ptr),
        1,
        "commit must establish exactly one receiver-side ORCA hold"
    );
    assert_eq!(
        runtime.borrow().actors[&receiver].mailbox.len(),
        0,
        "commit must consume exactly the selected message"
    );

    // Commit is single-use: replaying it cannot add another hold.
    callbacks.commit_receive_match();
    assert_eq!(
        foreign_count(ptr),
        1,
        "repeated commit without a reservation must be idempotent"
    );
}
