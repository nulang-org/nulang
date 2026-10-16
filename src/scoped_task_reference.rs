//! Deterministic, single-thread reference executor for the experimental
//! scoped-task protocol. Does not alter the `par` execution path.
//!
//! This executor is an executable reference for the event ordering and
//! cancellation contract defined by `TaskScope`. Callers supply a cooperative
//! callback that performs **at most one nonblocking step** of each admitted
//! task. No threads are spawned; a blocking callback would block this runner.
//!
//! A scheduler can compare its trace/outcomes against this reference for
//! equivalent ordered events, but this is not VM or WASM integration.

use crate::scoped_tasks::{
    CancellationCause, CancellationToken, ScopeError, TaskId, TaskOutcome, TaskScope,
};

/// Result of a single cooperative task step.
#[derive(Debug, PartialEq, Eq)]
pub enum TaskPoll<T, E> {
    /// The task is still executing and must be polled again in a later tick.
    Pending,
    /// The task has terminated normally or with an error.
    Ready(Result<T, E>),
    /// The task has actually terminated without producing a result.
    ///
    /// This must not be returned merely because cancellation was requested.
    Cancelled,
}

/// Actions applied by one bounded tick.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct TickStats {
    pub admitted: usize,
    pub polled: usize,
    pub completed: usize,
}

/// Reference host for the bounded scope contract.
///
/// It admits children in FIFO order and polls each active task once in
/// submission order per tick. A failure cancels pending children and signals
/// already-running siblings in the same tick. Results join in submission
/// order. The host never automatically acknowledges running-task cancellation.
#[derive(Debug)]
pub struct ReferenceExecutor<T, E> {
    scope: TaskScope<T, E>,
    active: Vec<TaskId>,
}

impl<T, E> ReferenceExecutor<T, E> {
    pub fn new(max_in_flight: usize, max_pending: usize) -> Result<Self, ScopeError> {
        Ok(Self {
            scope: TaskScope::new(max_in_flight, max_pending)?,
            active: Vec::new(),
        })
    }

    pub fn try_submit(&mut self) -> Result<TaskId, ScopeError> {
        self.scope.try_submit()
    }

    pub fn cancel(&mut self, reason: CancellationCause) {
        self.scope.cancel(reason);
    }

    pub fn cancellation(&self) -> Option<&CancellationCause> {
        self.scope.cancellation()
    }

    pub fn in_flight(&self) -> usize {
        self.scope.in_flight()
    }

    pub fn is_quiescent(&self) -> bool {
        self.scope.is_quiescent()
    }

    /// Execute exactly one finite cooperative scheduling tick.
    ///
    /// This function does not loop waiting for asynchronous progress. Callers
    /// are responsible for the number of ticks, virtual clock/deadlines, and
    /// for ensuring the supplied callback never blocks indefinitely.
    ///
    /// Newly available admission slots are refilled on the **next** tick,
    /// avoiding order changes based on callback completion timings.
    pub fn tick<F>(&mut self, mut poll: F) -> TickStats
    where
        F: FnMut(TaskId, &CancellationToken) -> TaskPoll<T, E>,
    {
        let mut stats = TickStats::default();
        while let Some(id) = self.scope.admit_next() {
            self.active.push(id);
            stats.admitted += 1;
        }

        let token = self.scope.cancellation_token();
        let mut index = 0;
        while index < self.active.len() {
            let id = self.active[index];
            stats.polled += 1;
            match poll(id, &token) {
                TaskPoll::Pending => index += 1,
                TaskPoll::Ready(result) => {
                    self.scope
                        .complete(id, result)
                        .expect("reference executor only polls running task IDs");
                    let _ = self.active.remove(index);
                    stats.completed += 1;
                }
                TaskPoll::Cancelled => {
                    self.scope
                        .acknowledge_cancelled(id)
                        .expect("reference executor only polls running task IDs");
                    let _ = self.active.remove(index);
                    stats.completed += 1;
                }
            }
        }
        stats
    }

    pub fn into_outcomes(self) -> Result<Vec<TaskOutcome<T, E>>, ScopeError> {
        self.scope.into_outcomes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scoped_tasks::{CancellationCause, ScopeError, TaskOutcome};

    #[test]
    fn only_admitted_children_are_polled_and_queue_capacity_is_respected() {
        let mut runner = ReferenceExecutor::<usize, &'static str>::new(1, 2).unwrap();
        let a = runner.try_submit().unwrap();
        let b = runner.try_submit().unwrap();
        assert_eq!(runner.try_submit(), Err(ScopeError::QueueFull));

        let first = runner.tick(|id, _| {
            assert_eq!(id, a);
            TaskPoll::Pending
        });
        assert_eq!(first.admitted, 1);
        assert_eq!(first.polled, 1);
        assert_eq!(runner.in_flight(), 1);
        let c = runner.try_submit().unwrap();
        assert_eq!(runner.try_submit(), Err(ScopeError::QueueFull));

        let second = runner.tick(|id, _| {
            assert_eq!(id, a);
            TaskPoll::Ready(Ok(10))
        });
        assert_eq!(second.admitted, 0);
        assert_eq!(runner.in_flight(), 0);

        let third = runner.tick(|id, _| {
            assert_eq!(id, b);
            TaskPoll::Ready(Ok(20))
        });
        assert_eq!(third.admitted, 1);
        let fourth = runner.tick(|id, _| {
            assert_eq!(id, c);
            TaskPoll::Ready(Ok(30))
        });
        assert_eq!(fourth.admitted, 1);
        assert!(runner.is_quiescent());
        assert_eq!(
            runner.into_outcomes().unwrap(),
            vec![
                TaskOutcome::Completed(Ok(10)),
                TaskOutcome::Completed(Ok(20)),
                TaskOutcome::Completed(Ok(30)),
            ]
        );
    }

    #[test]
    fn first_child_failure_signals_siblings_within_the_same_tick() {
        let mut runner = ReferenceExecutor::<(), &'static str>::new(2, 3).unwrap();
        let failing = runner.try_submit().unwrap();
        let sibling = runner.try_submit().unwrap();
        let _queued = runner.try_submit().unwrap();
        let mut sibling_observed_cancel = false;

        runner.tick(|id, token| {
            if id == failing {
                TaskPoll::Ready(Err("failure"))
            } else {
                assert_eq!(id, sibling);
                sibling_observed_cancel = token.is_cancelled();
                if token.is_cancelled() {
                    TaskPoll::Cancelled
                } else {
                    TaskPoll::Pending
                }
            }
        });
        assert!(sibling_observed_cancel);
        assert_eq!(
            runner.cancellation(),
            Some(&CancellationCause::ChildFailed(failing))
        );
        assert!(runner.is_quiescent());
        assert_eq!(
            runner.into_outcomes().unwrap(),
            vec![
                TaskOutcome::Completed(Err("failure")),
                TaskOutcome::Cancelled,
                TaskOutcome::Cancelled,
            ]
        );
    }

    #[test]
    fn parent_cancellation_before_first_tick_never_polls_children() {
        let mut runner = ReferenceExecutor::<(), ()>::new(2, 2).unwrap();
        runner.try_submit().unwrap();
        runner.try_submit().unwrap();
        runner.cancel(CancellationCause::Parent);
        let stats = runner.tick(|_, _| -> TaskPoll<(), ()> {
            panic!("pending work must not be polled after cancellation");
        });
        assert_eq!(stats.admitted, 0);
        assert_eq!(stats.polled, 0);
        assert!(runner.is_quiescent());
        assert_eq!(
            runner.into_outcomes().unwrap(),
            vec![TaskOutcome::Cancelled, TaskOutcome::Cancelled]
        );
    }

    #[test]
    fn pending_running_child_does_not_disappear_when_cancelled() {
        let mut runner = ReferenceExecutor::<(), ()>::new(1, 1).unwrap();
        let child = runner.try_submit().unwrap();
        runner.tick(|id, _| {
            assert_eq!(id, child);
            TaskPoll::Pending
        });
        runner.cancel(CancellationCause::DeadlineExceeded);
        let stats = runner.tick(|_, token| {
            assert!(token.is_cancelled());
            TaskPoll::Pending
        });
        assert_eq!(stats.completed, 0);
        assert!(!runner.is_quiescent());
        runner.tick(|_, token| {
            assert!(token.is_cancelled());
            TaskPoll::Cancelled
        });
        assert!(runner.is_quiescent());
        assert_eq!(
            runner.into_outcomes().unwrap(),
            vec![TaskOutcome::Cancelled]
        );
    }

    #[test]
    fn results_are_joined_in_submission_order_not_completion_order() {
        let mut runner = ReferenceExecutor::<usize, ()>::new(2, 2).unwrap();
        let a = runner.try_submit().unwrap();
        let b = runner.try_submit().unwrap();
        runner.tick(|id, _| {
            if id == a {
                TaskPoll::Pending
            } else if id == b {
                TaskPoll::Ready(Ok(2))
            } else {
                unreachable!()
            }
        });
        runner.tick(|id, _| {
            assert_eq!(id, a);
            TaskPoll::Ready(Ok(1))
        });
        assert_eq!(
            runner.into_outcomes().unwrap(),
            vec![TaskOutcome::Completed(Ok(1)), TaskOutcome::Completed(Ok(2))]
        );
    }

    #[test]
    fn tick_does_not_spin_on_tasks_that_remain_pending() {
        let mut runner = ReferenceExecutor::<usize, ()>::new(1, 1).unwrap();
        runner.try_submit().unwrap();
        for _ in 0..3 {
            let stats = runner.tick(|_, _| TaskPoll::Pending);
            assert_eq!(stats.polled, 1);
            assert_eq!(stats.completed, 0);
            assert!(!runner.is_quiescent());
        }
    }
}
