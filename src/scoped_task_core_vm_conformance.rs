//! Experimental CoreVM execution through the scoped-task reference contract.
//!
//! TEST ONLY: bytecode tasks are pure, non-suspending computations executed
//! synchronously on isolated VM instances. This is not parallel par lowering,
//! a cooperative VM instruction scheduler, or a production worker runtime.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scoped_task_reference::TickStats;
    use crate::scoped_tasks::{CancellationCause, TaskOutcome};

    #[test]
    fn bytecode_results_join_in_submission_order_despite_delayed_first_task() {
        let mut host = CoreVmTaskHost::new(2, 2);
        let first = host.submit_add(7, 5);
        let second = host.submit_add(20, 2);
        host.defer_once(first);

        assert_eq!(
            host.tick(),
            TickStats {
                admitted: 2,
                polled: 2,
                completed: 1,
            }
        );
        assert_eq!(host.executed(), &[second]);
        assert_eq!(
            host.tick(),
            TickStats {
                admitted: 0,
                polled: 1,
                completed: 1,
            }
        );
        assert_eq!(host.executed(), &[second, first]);
        assert_eq!(
            host.into_outcomes(),
            vec![TaskOutcome::Completed(Ok(12)), TaskOutcome::Completed(Ok(22))]
        );
    }

    #[test]
    fn parent_cancellation_prevents_queued_and_deferred_bytecode_from_running() {
        let mut host = CoreVmTaskHost::new(1, 2);
        let first = host.submit_add(100, 1);
        host.submit_add(200, 2);
        host.defer_once(first);

        assert_eq!(host.tick().polled, 1);
        assert!(host.executed().is_empty());

        host.cancel(CancellationCause::Parent);
        assert_eq!(host.tick().completed, 1);
        assert!(host.executed().is_empty());
        assert_eq!(
            host.into_outcomes(),
            vec![TaskOutcome::Cancelled, TaskOutcome::Cancelled]
        );
    }
}
