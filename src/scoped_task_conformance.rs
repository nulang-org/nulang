//! Test-only event trace conformance for the experimental scoped-task API.
//! Independent runtime backends can implement TraceHost to reuse these cases.

use crate::scoped_task_reference::{ReferenceExecutor, TaskPoll, TickStats};
use crate::scoped_tasks::{
    CancellationCause, CancellationToken, ScopeError, TaskId, TaskOutcome, TaskScope,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_fifo_and_submission_order_are_conformant() {
        let events = vec![
            TraceEvent::Submit,
            TraceEvent::Submit,
            TraceEvent::Submit,
            TraceEvent::Tick(vec![(0, Response::Pending), (1, Response::Success(20))]),
            TraceEvent::Submit,
            TraceEvent::Tick(vec![(0, Response::Success(10)), (2, Response::Pending)]),
            TraceEvent::Tick(vec![(2, Response::Success(30))]),
        ];
        let trace = assert_hosts_conform(2, 2, &events);
        assert_eq!(trace.records[2].submission, Some(Err(ScopeError::QueueFull)));
        assert_eq!(
            trace.outcomes,
            vec![
                TaskOutcome::Completed(Ok(10)),
                TaskOutcome::Completed(Ok(20)),
                TaskOutcome::Completed(Ok(30)),
            ]
        );
        assert!(trace.records.last().unwrap().state.quiescent);
    }

    #[test]
    fn first_failure_cancels_siblings_and_backlog_in_the_same_tick() {
        let events = vec![
            TraceEvent::Submit,
            TraceEvent::Submit,
            TraceEvent::Tick(vec![(0, Response::Pending), (1, Response::Pending)]),
            TraceEvent::Submit,
            TraceEvent::Tick(vec![(0, Response::Failure("boom")), (1, Response::Stop)]),
        ];
        let trace = assert_hosts_conform(2, 2, &events);
        assert_eq!(
            trace.records.last().unwrap().state.cancelled,
            Some(CancellationCause::ChildFailed(TaskId(0)))
        );
        assert_eq!(
            trace.outcomes,
            vec![
                TaskOutcome::Completed(Err("boom")),
                TaskOutcome::Cancelled,
                TaskOutcome::Cancelled,
            ]
        );
    }

    #[test]
    fn cancelled_parent_retains_running_child_until_it_acknowledges() {
        let events = vec![
            TraceEvent::Submit,
            TraceEvent::Tick(vec![(0, Response::Pending)]),
            TraceEvent::Submit,
            TraceEvent::Cancel(CancellationCause::DeadlineExceeded),
            TraceEvent::Tick(vec![(0, Response::Pending)]),
            TraceEvent::Tick(vec![(0, Response::Stop)]),
        ];
        let trace = assert_hosts_conform(1, 2, &events);
        assert_eq!(trace.records[3].state.running, 1);
        assert!(!trace.records[3].state.quiescent);
        assert_eq!(trace.records[4].tick.unwrap().completed, 0);
        assert!(trace.records.last().unwrap().state.quiescent);
        assert_eq!(
            trace.outcomes,
            vec![TaskOutcome::Cancelled, TaskOutcome::Cancelled]
        );
    }

    #[test]
    fn cancellation_before_admission_never_polls_queued_work() {
        let events = vec![
            TraceEvent::Submit,
            TraceEvent::Submit,
            TraceEvent::Cancel(CancellationCause::Parent),
            TraceEvent::Tick(vec![]),
        ];
        let trace = assert_hosts_conform(2, 2, &events);
        assert_eq!(trace.records[3].tick.unwrap().polled, 0);
        assert_eq!(
            trace.outcomes,
            vec![TaskOutcome::Cancelled, TaskOutcome::Cancelled]
        );
    }

    #[test]
    fn seeded_event_schedules_preserve_the_same_contract() {
        for seed in 0..64 {
            let events = seeded_events(seed);
            let trace = assert_hosts_conform(2, 3, &events);
            assert!(
                trace.records.last().unwrap().state.quiescent,
                "seed {seed} left running tasks"
            );
        }
    }
}
