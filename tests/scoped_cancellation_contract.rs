//! Public, transient scoped cancellation semantics. No actor interruption or durable replay.
use nulang::runtime::{CancellationReason, CancellationToken};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

#[test]
fn ancestor_cancellation_propagates_to_descendants_not_siblings() {
    let parent = CancellationToken::new();
    let a = parent.child();
    let a_child = a.child();
    let b = parent.child();
    let now = Instant::now();

    assert!(a.cancel());
    assert_eq!(a_child.check_at(now), Err(CancellationReason::Requested));
    assert_eq!(b.check_at(now), Ok(()));
    assert_eq!(parent.check_at(now), Ok(()));

    assert!(parent.cancel());
    assert_eq!(b.check_at(now), Err(CancellationReason::Requested));
    assert!(!parent.cancel());
}

#[test]
fn child_deadline_is_clamped_and_checked_at_exact_boundary() {
    let now = Instant::now();
    let root = CancellationToken::with_deadline(now + Duration::from_secs(10));
    let child = root.child_with_deadline(now + Duration::from_secs(30));
    let earlier = root.child_with_deadline(now + Duration::from_secs(3));

    assert_eq!(child.deadline(), root.deadline());
    assert_eq!(child.check_at(now + Duration::from_secs(9)), Ok(()));
    assert_eq!(
        child.check_at(now + Duration::from_secs(10)),
        Err(CancellationReason::DeadlineExceeded)
    );
    assert_eq!(
        earlier.check_at(now + Duration::from_secs(3)),
        Err(CancellationReason::DeadlineExceeded)
    );
    assert_eq!(root.check_at(now + Duration::from_secs(3)), Ok(()));
}

#[test]
fn children_retain_cancel_signal_after_parent_handle_is_dropped() {
    let root = CancellationToken::new();
    let observer = root.clone();
    let child = root.child().child();
    drop(root);
    assert!(observer.cancel());
    assert_eq!(
        child.check_at(Instant::now()),
        Err(CancellationReason::Requested)
    );
}

#[test]
fn concurrent_first_cancellation_has_a_single_winner() {
    let token = CancellationToken::new();
    let barrier = Arc::new(Barrier::new(8));
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let token = token.clone();
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                token.cancel()
            })
        })
        .collect();
    assert_eq!(
        handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .filter(|won| *won)
            .count(),
        1
    );
    assert_eq!(
        token.check_at(Instant::now()),
        Err(CancellationReason::Requested)
    );
}
