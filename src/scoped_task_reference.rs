//! Deterministic, single-thread reference executor for the experimental
//! scoped-task protocol. Does not alter the `par` execution path.

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
        assert_eq!(runner.into_outcomes().unwrap(), vec![TaskOutcome::Cancelled]);
    }

    #[test]
    fn results_are_joined_in_submission_order_not_completion_order() {
        let mut runner = ReferenceExecutor::<usize, ()>::new(2, 2).unwrap();
        let a = runner.try_submit().unwrap();
        let b = runner.try_submit().unwrap();
        runner.tick(|id, _| {
            if id == a { TaskPoll::Pending }
            else if id == b { TaskPoll::Ready(Ok(2)) }
            else { unreachable!() }
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
