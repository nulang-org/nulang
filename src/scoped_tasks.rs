//! Executor-neutral admission and cancellation contract for scoped child tasks.
//!
//! This module deliberately does not launch threads or change `par` lowering:
//! execution backends drive admission and acknowledge completion/cancellation.
//! Both running tasks and queued tasks are bounded, preventing an unbounded
//! backlog. No child is considered terminated until its executor reports a
//! completion or a cancellation acknowledgement.
//!
//! This is an event-driven state machine: the same *ordered events* produce
//! the same results on any host. Which concurrent failure is observed first
//! is chosen by the host's event ordering, not by this module. Real schedulers
//! must propagate cancellation to already-running child computations.
//!
//! This module is experimental and does not alter source language semantics.

use std::collections::VecDeque;

/// Stable identity within a single parent scope, in submission order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TaskId(pub usize);

/// The first cancellation reason wins; later reasons cannot overwrite it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancellationCause {
    Parent,
    DeadlineExceeded,
    ChildFailed(TaskId),
}

/// An individual task's final result.
#[derive(Debug, PartialEq, Eq)]
pub enum TaskOutcome<T, E> {
    Completed(Result<T, E>),
    Cancelled,
}

/// Invalid admission or lifecycle transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeError {
    ZeroLimit,
    QueueFull,
    Cancelled,
    UnknownTask(TaskId),
    NotRunning(TaskId),
    NotQuiescent,
}

#[derive(Debug)]
enum TaskSlot<T, E> {
    Pending,
    Running,
    Terminal(TaskOutcome<T, E>),
}

/// Pure coordinator for an executor-managed lexical scope.
///
/// `max_in_flight` bounds admitted children; `max_pending` bounds children
/// awaiting admission. `try_submit` is nonblocking and fails at the backlog
/// limit. Hosts may retry submission only after observing admission progress.
///
/// Cancellation discards pending children, prevents future admission, and
/// retains active children until they complete or acknowledge cancellation.
/// This is distinct from stopping an independently addressable Nulang actor.
#[derive(Debug)]
pub struct TaskScope<T, E> {
    max_in_flight: usize,
    max_pending: usize,
    pending: VecDeque<TaskId>,
    slots: Vec<TaskSlot<T, E>>,
    in_flight: usize,
    cancellation: Option<CancellationCause>,
}

impl<T, E> TaskScope<T, E> {
    pub fn new(max_in_flight: usize, max_pending: usize) -> Result<Self, ScopeError> {
        if max_in_flight == 0 || max_pending == 0 {
            return Err(ScopeError::ZeroLimit);
        }
        Ok(Self {
            max_in_flight,
            max_pending,
            pending: VecDeque::new(),
            slots: Vec::new(),
            in_flight: 0,
            cancellation: None,
        })
    }

    /// Register a child without executing it or blocking the caller.
    pub fn try_submit(&mut self) -> Result<TaskId, ScopeError> {
        if self.cancellation.is_some() {
            return Err(ScopeError::Cancelled);
        }
        if self.pending.len() >= self.max_pending {
            return Err(ScopeError::QueueFull);
        }
        let id = TaskId(self.slots.len());
        self.slots.push(TaskSlot::Pending);
        self.pending.push_back(id);
        Ok(id)
    }

    /// Admit the oldest pending child only if execution capacity is available.
    pub fn admit_next(&mut self) -> Option<TaskId> {
        if self.cancellation.is_some() || self.in_flight >= self.max_in_flight {
            return None;
        }
        let id = self.pending.pop_front()?;
        debug_assert!(matches!(self.slots.get(id.0), Some(TaskSlot::Pending)));
        self.slots[id.0] = TaskSlot::Running;
        self.in_flight += 1;
        Some(id)
    }

    /// Report a completed child. The first observed failure cancels siblings.
    ///
    /// The executor must signal active siblings separately; they retain
    /// their slots until completion or cancellation is acknowledged.
    pub fn complete(&mut self, id: TaskId, result: Result<T, E>) -> Result<(), ScopeError> {
        self.ensure_running(id)?;
        if result.is_err() && self.cancellation.is_none() {
            self.cancel(CancellationCause::ChildFailed(id));
        }
        self.slots[id.0] = TaskSlot::Terminal(TaskOutcome::Completed(result));
        self.in_flight -= 1;
        Ok(())
    }

    /// A child that was running acknowledges it has terminated after cancel.
    pub fn acknowledge_cancelled(&mut self, id: TaskId) -> Result<(), ScopeError> {
        self.ensure_running(id)?;
        self.slots[id.0] = TaskSlot::Terminal(TaskOutcome::Cancelled);
        self.in_flight -= 1;
        Ok(())
    }

    /// Signal cancellation once. A host must subsequently interrupt any
    /// already-running children and report their terminal status.
    pub fn cancel(&mut self, cause: CancellationCause) {
        if self.cancellation.is_some() {
            return;
        }
        self.cancellation = Some(cause);
        for slot in &mut self.slots {
            if matches!(slot, TaskSlot::Pending) {
                *slot = TaskSlot::Terminal(TaskOutcome::Cancelled);
            }
        }
        self.pending.clear();
    }

    pub fn cancellation(&self) -> Option<&CancellationCause> {
        self.cancellation.as_ref()
    }

    pub fn pending(&self) -> usize {
        self.pending.len()
    }

    pub fn in_flight(&self) -> usize {
        self.in_flight
    }

    /// A scope may close only when no child is queued or still running.
    pub fn is_quiescent(&self) -> bool {
        self.pending.is_empty() && self.in_flight == 0
    }

    /// Consume terminal results in deterministic submission order.
    ///
    /// Call after `is_quiescent`. This consumes the scope even on error.
    pub fn into_outcomes(self) -> Result<Vec<TaskOutcome<T, E>>, ScopeError> {
        if !self.is_quiescent() {
            return Err(ScopeError::NotQuiescent);
        }
        Ok(self
            .slots
            .into_iter()
            .map(|slot| match slot {
                TaskSlot::Terminal(outcome) => outcome,
                TaskSlot::Pending | TaskSlot::Running => unreachable!("non-quiescent child"),
            })
            .collect())
    }

    fn ensure_running(&self, id: TaskId) -> Result<(), ScopeError> {
        match self.slots.get(id.0) {
            None => Err(ScopeError::UnknownTask(id)),
            Some(TaskSlot::Running) => Ok(()),
            Some(_) => Err(ScopeError::NotRunning(id)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_limits_are_rejected() {
        assert!(matches!(
            TaskScope::<usize, &'static str>::new(0, 1),
            Err(ScopeError::ZeroLimit)
        ));
        assert!(matches!(
            TaskScope::<usize, &'static str>::new(1, 0),
            Err(ScopeError::ZeroLimit)
        ));
    }

    #[test]
    fn admission_is_fifo_with_bounded_in_flight_and_backlog() {
        let mut scope = TaskScope::<usize, &'static str>::new(2, 2).unwrap();
        let a = scope.try_submit().unwrap();
        let b = scope.try_submit().unwrap();
        assert_eq!(scope.try_submit(), Err(ScopeError::QueueFull));
        assert_eq!(scope.admit_next(), Some(a));
        assert_eq!(scope.admit_next(), Some(b));
        assert_eq!(scope.admit_next(), None);
        assert_eq!(scope.in_flight(), 2);

        let c = scope.try_submit().unwrap();
        let d = scope.try_submit().unwrap();
        assert_eq!(scope.try_submit(), Err(ScopeError::QueueFull));
        scope.complete(b, Ok(20)).unwrap();
        assert_eq!(scope.admit_next(), Some(c));
        assert_eq!(scope.in_flight(), 2);

        scope.complete(a, Ok(10)).unwrap();
        assert_eq!(scope.admit_next(), Some(d));
        scope.complete(d, Ok(40)).unwrap();
        scope.complete(c, Ok(30)).unwrap();
        assert!(scope.is_quiescent());
        assert_eq!(
            scope.into_outcomes().unwrap(),
            vec![
                TaskOutcome::Completed(Ok(10)),
                TaskOutcome::Completed(Ok(20)),
                TaskOutcome::Completed(Ok(30)),
                TaskOutcome::Completed(Ok(40)),
            ]
        );
    }

    #[test]
    fn first_error_cancels_pending_and_waits_for_running_siblings() {
        let mut scope = TaskScope::<usize, &'static str>::new(2, 3).unwrap();
        let a = scope.try_submit().unwrap();
        let b = scope.try_submit().unwrap();
        let _pending = scope.try_submit().unwrap();
        assert_eq!(scope.admit_next(), Some(a));
        assert_eq!(scope.admit_next(), Some(b));
        scope.complete(b, Err("failed")).unwrap();

        assert_eq!(scope.cancellation(), Some(&CancellationCause::ChildFailed(b)));
        assert_eq!(scope.pending(), 0);
        assert_eq!(scope.in_flight(), 1);
        assert_eq!(scope.admit_next(), None);
        assert_eq!(scope.try_submit(), Err(ScopeError::Cancelled));
        assert!(!scope.is_quiescent());

        scope.acknowledge_cancelled(a).unwrap();
        assert!(scope.is_quiescent());
        assert_eq!(
            scope.into_outcomes().unwrap(),
            vec![
                TaskOutcome::Cancelled,
                TaskOutcome::Completed(Err("failed")),
                TaskOutcome::Cancelled,
            ]
        );
    }

    #[test]
    fn parent_cancel_and_deadline_are_sticky_and_keep_running_work_alive() {
        let mut scope = TaskScope::<(), ()>::new(1, 2).unwrap();
        let running = scope.try_submit().unwrap();
        let _pending = scope.try_submit().unwrap();
        scope.admit_next();
        scope.cancel(CancellationCause::Parent);
        scope.cancel(CancellationCause::DeadlineExceeded);

        assert_eq!(scope.cancellation(), Some(&CancellationCause::Parent));
        assert_eq!(scope.in_flight(), 1);
        assert!(!scope.is_quiescent());
        scope.complete(running, Ok(())).unwrap();
        assert_eq!(
            scope.into_outcomes().unwrap(),
            vec![
                TaskOutcome::Completed(Ok(())),
                TaskOutcome::Cancelled,
            ]
        );
    }

    #[test]
    fn cancellation_can_be_acknowledged_and_duplicate_or_unknown_ids_are_rejected() {
        let mut scope = TaskScope::<usize, &'static str>::new(1, 1).unwrap();
        let a = scope.try_submit().unwrap();
        assert_eq!(scope.complete(a, Ok(1)), Err(ScopeError::NotRunning(a)));
        assert_eq!(
            scope.acknowledge_cancelled(TaskId(999)),
            Err(ScopeError::UnknownTask(TaskId(999)))
        );
        scope.admit_next();
        scope.cancel(CancellationCause::DeadlineExceeded);
        scope.acknowledge_cancelled(a).unwrap();
        assert_eq!(
            scope.acknowledge_cancelled(a),
            Err(ScopeError::NotRunning(a))
        );
        assert!(scope.is_quiescent());
        assert_eq!(scope.into_outcomes().unwrap(), vec![TaskOutcome::Cancelled]);
    }

    #[test]
    fn cancellation_before_admission_terminalizes_all_pending_children() {
        let mut scope = TaskScope::<usize, &'static str>::new(2, 2).unwrap();
        scope.try_submit().unwrap();
        scope.try_submit().unwrap();
        scope.cancel(CancellationCause::DeadlineExceeded);
        assert_eq!(scope.admit_next(), None);
        assert_eq!(scope.in_flight(), 0);
        assert!(scope.is_quiescent());
        assert_eq!(
            scope.into_outcomes().unwrap(),
            vec![TaskOutcome::Cancelled, TaskOutcome::Cancelled]
        );
    }

    #[test]
    fn later_failures_do_not_overwrite_initial_parent_cancellation() {
        let mut scope = TaskScope::<usize, &'static str>::new(2, 2).unwrap();
        let a = scope.try_submit().unwrap();
        let b = scope.try_submit().unwrap();
        scope.admit_next();
        scope.admit_next();
        scope.cancel(CancellationCause::Parent);
        scope.complete(b, Err("late")).unwrap();
        assert_eq!(scope.cancellation(), Some(&CancellationCause::Parent));
        scope.acknowledge_cancelled(a).unwrap();
        assert!(scope.is_quiescent());
    }

    #[test]
    fn completion_does_not_mean_parent_scope_can_close_with_queued_children() {
        let mut scope = TaskScope::<usize, ()>::new(1, 2).unwrap();
        let a = scope.try_submit().unwrap();
        let _b = scope.try_submit().unwrap();
        scope.admit_next();
        scope.complete(a, Ok(1)).unwrap();
        assert!(!scope.is_quiescent());
        assert_eq!(scope.pending(), 1);
    }
}
