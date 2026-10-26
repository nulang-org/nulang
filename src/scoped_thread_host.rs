//! Opt-in native structured thread scope, using the canonical TaskScope.
//!
//! This host is not the language `par` scheduler. Jobs run concurrently on
//! bounded OS threads, with synchronous lexical joining. Cancellation is
//! cooperative: blocking calls cannot be preempted and must eventually return.

use crate::runtime::CancellationToken;
use crate::scoped_tasks::{
    CancellationCause, ScopeError, TaskCancellation, TaskId, TaskOutcome, TaskScope,
};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::mpsc;
use std::thread;

/// A task's final state, reported only once its callback has returned.
#[derive(Debug)]
pub enum ScopedThreadExit<T, E> {
    Completed(Result<T, E>),
    Cancelled,
}

/// A possibly borrowing, one-shot child callback.
///
/// Captured stack references are safe because `run_scoped_threads` joins
/// every launched worker before returning.
pub type ScopedThreadTask<'a, T, E> =
    Box<dyn FnOnce(TaskCancellation) -> ScopedThreadExit<T, E> + Send + 'a>;

/// Lifecycle errors that cannot be represented as ordinary task outcomes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopedThreadError {
    Scope(ScopeError),
    ChildPanicked(TaskId),
}

impl From<ScopeError> for ScopedThreadError {
    fn from(value: ScopeError) -> Self {
        Self::Scope(value)
    }
}

/// Run a fully bounded set of concurrent native children, joining on exit.
///
/// `max_in_flight` bounds simultaneous workers. `max_pending` bounds the
/// *entire* initial batch; overflow fails before launching any child.
/// Results remain in source submission order regardless of completion order.
///
/// Child errors cancel queued tasks immediately and signal running workers.
/// A child panic also signals siblings and is returned *after* every launched
/// worker has joined. A worker reports `Cancelled` only by actually returning
/// from its callback. Parent cancellation is inherited without giving child
/// callbacks permission to cancel the parent or their siblings.
///
/// This does not preempt blocking native calls, implement VM/WASM worker
/// scheduling, make source `par` concurrent, or provide durable task receipts.
pub fn run_scoped_threads<'a, T, E>(
    tasks: Vec<ScopedThreadTask<'a, T, E>>,
    max_in_flight: usize,
    max_pending: usize,
    parent: &CancellationToken,
) -> Result<Vec<TaskOutcome<T, E>>, ScopedThreadError>
where
    T: Send,
    E: Send,
{
    let mut scope = TaskScope::with_parent(max_in_flight, max_pending, parent)?;
    if tasks.len() > max_pending {
        return Err(ScopeError::QueueFull.into());
    }
    for _ in &tasks {
        scope.try_submit()?;
    }

    thread::scope(|threads| {
        // Only the coordinator mutates child lifecycle. Worker threads send
        // terminal messages; the coordinator owns and joins all handles.
        let (sender, receiver) = mpsc::channel();
        let mut jobs: Vec<_> = tasks.into_iter().map(Some).collect();
        let mut handles = Vec::new();
        let mut first_panic = None;

        while !scope.is_quiescent() {
            while let Some(id) = scope.admit_next() {
                let task = jobs[id.0].take().expect("a child launches at most once");
                let token = scope.cancellation_token();
                let sender = sender.clone();
                let handle = threads.spawn(move || {
                    // Catch user callback panics so coordinator cancellation
                    // happens before the host begins joining other workers.
                    let done = catch_unwind(AssertUnwindSafe(|| task(token)));
                    let _ = sender.send((id, done));
                });
                handles.push((id, handle));
            }

            if scope.is_quiescent() {
                break;
            }
            let (id, result) = receiver
                .recv()
                .expect("an admitted worker must send exactly one terminal message");
            match result {
                Ok(ScopedThreadExit::Completed(result)) => scope.complete(id, result)?,
                Ok(ScopedThreadExit::Cancelled) => scope.acknowledge_cancelled(id)?,
                Err(_) => {
                    if first_panic.is_none() {
                        first_panic = Some(id);
                    }
                    scope.cancel(CancellationCause::ChildFailed(id));
                    scope.acknowledge_cancelled(id)?;
                }
            }
        }

        // Sending a terminal message precedes actual OS-thread termination;
        // join all handles before exposing outcomes to the caller.
        for (id, handle) in handles {
            if handle.join().is_err() && first_panic.is_none() {
                first_panic = Some(id);
            }
        }
        if let Some(id) = first_panic {
            return Err(ScopedThreadError::ChildPanicked(id));
        }
        scope.try_join().map_err(Into::into)
    })
}
