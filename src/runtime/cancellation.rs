//! Transient scoped-task cancellation contract (RFC 0024).
//!
//! Cancellation is cooperative. A token carries scope-local cancellation and
//! an optional monotonic deadline, with parent-to-child propagation.
//! This module does not suspend an actor, park a scheduler thread, or journal
//! a durable workflow cancellation.

#[cfg(test)]
mod tests {
    use super::{CancellationReason, CancellationToken};
    use std::time::{Duration, Instant};

    #[test]
    fn fresh_scope_is_active_and_has_no_implicit_deadline() {
        let token = CancellationToken::new();
        let now = Instant::now();
        assert_eq!(token.deadline(), None);
        assert_eq!(token.check_at(now), Ok(()));
        assert!(!token.is_cancelled_at(now));
    }

    #[test]
    fn clones_share_cancellation_and_cancel_is_idempotent() {
        let token = CancellationToken::new();
        let cloned = token.clone();
        let now = Instant::now();
        assert!(cloned.cancel(), "first request transitions the shared scope");
        assert!(!token.cancel(), "repeated request is a no-op");
        assert_eq!(token.check_at(now), Err(CancellationReason::Requested));
        assert_eq!(cloned.check_at(now), Err(CancellationReason::Requested));
    }

    #[test]
    fn cancellation_propagates_to_children_but_not_parents_or_siblings() {
        let root = CancellationToken::new();
        let first = root.child();
        let second = root.child();
        let grandchild = first.child();
        let now = Instant::now();

        first.cancel();
        assert_eq!(grandchild.check_at(now), Err(CancellationReason::Requested));
        assert_eq!(root.check_at(now), Ok(()));
        assert_eq!(second.check_at(now), Ok(()));

        root.cancel();
        assert_eq!(second.check_at(now), Err(CancellationReason::Requested));
    }

    #[test]
    fn child_deadline_cannot_extend_parent_deadline() {
        let now = Instant::now();
        let parent_deadline = now + Duration::from_secs(10);
        let root = CancellationToken::with_deadline(parent_deadline);
        let child = root.child_with_deadline(now + Duration::from_secs(30));

        assert_eq!(child.deadline(), Some(parent_deadline));
        assert_eq!(child.check_at(parent_deadline), Err(CancellationReason::DeadlineExceeded));
    }

    #[test]
    fn earlier_child_deadline_does_not_cancel_parent_or_siblings() {
        let now = Instant::now();
        let root = CancellationToken::with_deadline(now + Duration::from_secs(30));
        let child = root.child_with_deadline(now + Duration::from_secs(5));
        let sibling = root.child();
        let at = now + Duration::from_secs(5);
        assert_eq!(child.deadline(), Some(at));
        assert_eq!(child.check_at(at), Err(CancellationReason::DeadlineExceeded));
        assert_eq!(root.check_at(at), Ok(()));
        assert_eq!(sibling.check_at(at), Ok(()));
    }

    #[test]
    fn deadline_expires_on_boundary_without_sleeping() {
        let now = Instant::now();
        let deadline = now + Duration::from_secs(7);
        let token = CancellationToken::with_deadline(deadline);
        assert_eq!(token.check_at(now + Duration::from_secs(6)), Ok(()));
        assert_eq!(token.check_at(deadline), Err(CancellationReason::DeadlineExceeded));
        assert_eq!(token.check_at(now + Duration::from_secs(8)), Err(CancellationReason::DeadlineExceeded));
    }

    #[test]
    fn explicit_cancel_takes_precedence_over_elapsed_deadline() {
        let now = Instant::now();
        let root = CancellationToken::with_deadline(now + Duration::from_secs(1));
        let child = root.child();
        root.cancel();
        assert_eq!(
            child.check_at(now + Duration::from_secs(2)),
            Err(CancellationReason::Requested)
        );
    }

    #[test]
    fn child_keeps_parent_cancellation_state_alive_after_parent_handle_drops() {
        let root = CancellationToken::new();
        let parent = root.clone();
        let child = parent.child();
        drop(parent);
        root.cancel();
        assert_eq!(
            child.check_at(Instant::now()),
            Err(CancellationReason::Requested)
        );
    }

    #[test]
    fn cancellation_tokens_can_cross_worker_thread_boundaries() {
        fn assert_send_sync<T: Send + Sync + 'static>() {}
        assert_send_sync::<CancellationToken>();
        let token = CancellationToken::new();
        let other = token.clone();
        std::thread::spawn(move || other.cancel()).join().unwrap();
        assert_eq!(
            token.check_at(Instant::now()),
            Err(CancellationReason::Requested)
        );
    }
}
