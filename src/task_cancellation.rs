//! Portable, cancellation-only primitive shared by interpreters and task scopes.
//!
//! Does not depend on OS threads, actor scheduler, JIT, or native-only crates.
//! It can be compiled into the browser playground without the task executor.

use std::sync::{
    atomic::{AtomicU8, Ordering},
    Arc,
};
use std::time::Instant;

const ACTIVE: u8 = 0;
const CANCELLED: u8 = 1;
const DEADLINE: u8 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CancellationReason {
    Cancelled,
    DeadlineExceeded,
}

#[derive(Debug)]
struct CancellationState {
    state: AtomicU8,
    deadline: Option<Instant>,
}

/// Cooperative cancellation shared by one task scope and its descendants.
///
/// A checkpoint consults a monotonic deadline, if configured. Cancellation
/// never kills a thread or preempts blocking syscalls; children are responsible
/// for checking the token between bounded pieces of work.
#[derive(Clone, Debug)]
pub struct CancellationToken {
    state: Arc<CancellationState>,
}

impl Default for CancellationToken {
    fn default() -> Self {
        Self {
            state: Arc::new(CancellationState {
                state: AtomicU8::new(ACTIVE),
                deadline: None,
            }),
        }
    }
}

impl CancellationToken {
    pub fn with_deadline(deadline: Instant) -> Self {
        Self {
            state: Arc::new(CancellationState {
                state: AtomicU8::new(ACTIVE),
                deadline: Some(deadline),
            }),
        }
    }

    pub fn cancel(&self) {
        // A deadline that has already elapsed must not be overwritten by
        // a later explicit cancellation that happens before a checkpoint.
        let _ = self.cancellation_reason();
        let _ = self.state.state.compare_exchange(
            ACTIVE,
            CANCELLED,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    /// Report the earliest cancellation cause. Once cancelled, the cause is
    /// stable: a later deadline cannot replace an explicit cancellation.
    pub fn cancellation_reason(&self) -> Option<CancellationReason> {
        if self
            .state
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            let _ = self.state.state.compare_exchange(
                ACTIVE,
                DEADLINE,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
        }
        match self.state.state.load(Ordering::Acquire) {
            CANCELLED => Some(CancellationReason::Cancelled),
            DEADLINE => Some(CancellationReason::DeadlineExceeded),
            _ => None,
        }
    }

    /// Call at explicit safe points in child work.
    pub fn checkpoint(&self) -> Result<(), CancellationReason> {
        match self.cancellation_reason() {
            Some(reason) => Err(reason),
            None => Ok(()),
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancellation_reason().is_some()
    }
}

/// Stable, host-neutral diagnostic for a cooperatively cancelled run.
pub fn cancellation_error_message(reason: CancellationReason) -> &'static str {
    match reason {
        CancellationReason::Cancelled => "Task cancelled",
        CancellationReason::DeadlineExceeded => "Task deadline exceeded",
    }
}
