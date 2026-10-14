//! Transient scoped-task cancellation contract (RFC 0024).
//!
//! Cancellation is cooperative. A token carries scope-local cancellation and
//! an optional monotonic deadline, with parent-to-child propagation.
//! This module does not suspend an actor, park a scheduler thread, or journal
//! a durable workflow cancellation.

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

/// Why a transient scoped operation should stop. A requested cancellation
/// takes precedence over an elapsed deadline when both are observable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancellationReason {
    Requested,
    DeadlineExceeded,
}

impl fmt::Display for CancellationReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Requested => f.write_str("operation cancelled"),
            Self::DeadlineExceeded => f.write_str("operation deadline exceeded"),
        }
    }
}

impl std::error::Error for CancellationReason {}

#[derive(Debug)]
struct CancellationState {
    requested: AtomicBool,
    // A strong parent link retains the cancellation signal even when the
    // lexical parent handle is dropped. Children do not retain one another.
    parent: Option<CancellationToken>,
    // Effective deadline: inherited minimum, if any.
    deadline: Option<Instant>,
}

/// A cheap, thread-safe, cooperative cancellation handle for transient work.
///
/// Clones refer to the same scope. Children observe ancestor cancellation and
/// cannot extend an inherited deadline. A child's cancel request never
/// cancels the parent or its sibling scopes.
///
/// This is not an actor termination signal or a durable workflow command.
/// Consumers must check the token at well-defined suspension/admission points.
/// Deadline checks use a monotonic Instant; `check_at` accepts a supplied
/// clock value so replay/simulation tests do not sleep.
#[derive(Debug, Clone)]
pub struct CancellationToken {
    inner: Arc<CancellationState>,
}

impl Default for CancellationToken {
    fn default() -> Self {
        Self::new()
    }
}

impl CancellationToken {
    /// Create an independent, active scope with no deadline.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(CancellationState {
                requested: AtomicBool::new(false),
                parent: None,
                deadline: None,
            }),
        }
    }

    /// Create an independent scope whose deadline is monotonic.
    pub fn with_deadline(deadline: Instant) -> Self {
        Self {
            inner: Arc::new(CancellationState {
                requested: AtomicBool::new(false),
                parent: None,
                deadline: Some(deadline),
            }),
        }
    }

    /// Create a child scope inheriting its parent's effective deadline.
    pub fn child(&self) -> Self {
        self.linked_child(self.deadline())
    }

    /// Create a child with a deadline no later than its parent's.
    pub fn child_with_deadline(&self, deadline: Instant) -> Self {
        let effective = self.deadline().map_or(deadline, |parent| parent.min(deadline));
        self.linked_child(Some(effective))
    }

    fn linked_child(&self, deadline: Option<Instant>) -> Self {
        Self {
            inner: Arc::new(CancellationState {
                requested: AtomicBool::new(false),
                parent: Some(self.clone()),
                deadline,
            }),
        }
    }

    /// The effective deadline, after inheriting any parent constraint.
    pub fn deadline(&self) -> Option<Instant> {
        self.inner.deadline
    }

    /// Request cancellation for this scope and its descendants.
    ///
    /// Returns true only on the first transition for this scope. This method
    /// does not interrupt a running worker or awaken a parked scheduler task.
    pub fn cancel(&self) -> bool {
        !self.inner.requested.swap(true, Ordering::AcqRel)
    }

    /// Poll cancellation and deadline expiry at an explicitly supplied time.
    ///
    /// Traverses parent links iteratively so deeply nested scopes do not
    /// overflow the stack. Explicit cancellation dominates elapsed deadlines.
    pub fn check_at(&self, now: Instant) -> Result<(), CancellationReason> {
        let mut scope = Some(self);
        while let Some(token) = scope {
            if token.inner.requested.load(Ordering::Acquire) {
                return Err(CancellationReason::Requested);
            }
            scope = token.inner.parent.as_ref();
        }
        if self.inner.deadline.is_some_and(|deadline| now >= deadline) {
            Err(CancellationReason::DeadlineExceeded)
        } else {
            Ok(())
        }
    }

    /// Poll using the host monotonic clock. Do not call during deterministic
    /// durable replay; use an injected clock via `check_at` instead.
    pub fn check(&self) -> Result<(), CancellationReason> {
        self.check_at(Instant::now())
    }

    pub fn is_cancelled_at(&self, now: Instant) -> bool {
        self.check_at(now).is_err()
    }

    pub fn is_cancelled(&self) -> bool {
        self.check().is_err()
    }
}

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
        assert_eq!(
            token.check_at(now + Duration::from_secs(8)),
            Err(CancellationReason::DeadlineExceeded)
        );
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
