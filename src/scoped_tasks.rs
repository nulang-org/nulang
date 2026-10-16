//! Executor-neutral admission and cancellation contract for scoped child tasks.
//!
//! This module deliberately does not launch threads or change `par` lowering:
//! execution backends drive admission and acknowledge completion/cancellation.

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
        assert_eq!(scope.acknowledge_cancelled(a), Err(ScopeError::NotRunning(a)));
        assert!(scope.is_quiescent());
        assert_eq!(scope.into_outcomes().unwrap(), vec![TaskOutcome::Cancelled]);
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
