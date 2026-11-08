//! Black-box contracts for the experimental executor-neutral scoped task coordinator.
use nulang::runtime::{CancellationReason, CancellationToken};
use nulang::scoped_task_reference::{ReferenceExecutor, TaskPoll};
use nulang::scoped_tasks::{CancellationCause, ScopeError, TaskOutcome, TaskScope};
use std::time::{Duration, Instant};

#[test]
fn parent_cancellation_stops_admission_and_requires_running_termination_ack() {
    let parent = CancellationToken::new();
    let mut scope = TaskScope::<usize, &'static str>::with_parent(1, 2, &parent).unwrap();
    let active = scope.try_submit().unwrap();
    let _queued = scope.try_submit().unwrap();
    assert_eq!(scope.admit_next(), Some(active));
    let child = scope.cancellation_token();

    parent.cancel();
    assert_eq!(scope.try_submit(), Err(ScopeError::Cancelled));
    assert_eq!(scope.pending(), 0);
    assert_eq!(scope.cancellation(), Some(&CancellationCause::Parent));
    assert_eq!(
        child.check_at(Instant::now()),
        Err(CancellationReason::Requested)
    );
    assert!(!scope.is_quiescent());
    assert_eq!(scope.try_join(), Err(ScopeError::NotQuiescent));

    scope.acknowledge_cancelled(active).unwrap();
    assert_eq!(
        scope.try_join().unwrap(),
        vec![TaskOutcome::Cancelled, TaskOutcome::Cancelled]
    );
}

#[test]
fn inherited_deadline_does_not_cancel_unrelated_siblings() {
    let now = Instant::now();
    let parent = CancellationToken::with_deadline(now + Duration::from_secs(5));
    let mut first = TaskScope::<(), ()>::with_parent(1, 1, &parent).unwrap();
    let mut second = TaskScope::<(), ()>::new(1, 1).unwrap();
    first.try_submit().unwrap();
    second.try_submit().unwrap();

    first.observe_cancellation_at(now + Duration::from_secs(5));
    second.observe_cancellation_at(now + Duration::from_secs(5));
    assert_eq!(
        first.cancellation(),
        Some(&CancellationCause::DeadlineExceeded)
    );
    assert!(first.is_quiescent());
    assert_eq!(second.cancellation(), None);
    assert_eq!(second.pending(), 1);
}

#[test]
fn failure_cancels_queued_and_running_siblings_without_claiming_ack() {
    let mut scope = TaskScope::<u32, &'static str>::new(2, 3).unwrap();
    let failing = scope.try_submit().unwrap();
    let active = scope.try_submit().unwrap();
    let _queued = scope.try_submit().unwrap();
    assert_eq!(scope.admit_next(), Some(failing));
    assert_eq!(scope.admit_next(), Some(active));
    let child = scope.cancellation_token();

    scope.complete(failing, Err("failed")).unwrap();
    assert!(child.is_cancelled());
    assert_eq!(
        scope.cancellation(),
        Some(&CancellationCause::ChildFailed(failing))
    );
    assert!(!scope.is_quiescent());
    assert_eq!(scope.try_join(), Err(ScopeError::NotQuiescent));
    scope.acknowledge_cancelled(active).unwrap();
    assert_eq!(
        scope.try_join().unwrap(),
        vec![
            TaskOutcome::Completed(Err("failed")),
            TaskOutcome::Cancelled,
            TaskOutcome::Cancelled,
        ]
    );
}

#[test]
fn reference_executor_drains_in_source_order_and_only_after_ack() {
    let mut executor = ReferenceExecutor::<u32, &'static str>::new(2, 2).unwrap();
    let first = executor.try_submit().unwrap();
    let second = executor.try_submit().unwrap();
    let mut first_polled = false;
    let stats = executor.tick(|id, cancellation| {
        assert!(!cancellation.is_cancelled());
        if id == first {
            first_polled = true;
            TaskPoll::Pending
        } else {
            assert_eq!(id, second);
            TaskPoll::Ready(Ok(2))
        }
    });
    assert_eq!(stats.polled, 2);
    assert!(first_polled);
    assert_eq!(executor.try_join(), Err(ScopeError::NotQuiescent));

    executor.tick(|id, _| {
        assert_eq!(id, first);
        TaskPoll::Ready(Ok(1))
    });
    assert_eq!(
        executor.try_join().unwrap(),
        vec![TaskOutcome::Completed(Ok(1)), TaskOutcome::Completed(Ok(2))]
    );
}

#[test]
fn successful_join_closes_scope_and_does_not_reopen_admission() {
    let mut scope = TaskScope::<usize, ()>::new(1, 1).unwrap();
    let id = scope.try_submit().unwrap();
    assert_eq!(scope.admit_next(), Some(id));
    scope.complete(id, Ok(7)).unwrap();
    assert_eq!(
        scope.try_join().unwrap(),
        vec![TaskOutcome::Completed(Ok(7))]
    );
    assert_eq!(scope.try_submit(), Err(ScopeError::Closed));
    assert_eq!(scope.admit_next(), None);
    assert_eq!(scope.try_join(), Err(ScopeError::Closed));
}

#[test]
fn failing_child_must_not_cancel_parent_or_other_scopes() {
    let parent = CancellationToken::new();
    let mut failing = TaskScope::<(), &'static str>::with_parent(1, 1, &parent).unwrap();
    let mut unrelated = TaskScope::<(), &'static str>::with_parent(1, 1, &parent).unwrap();
    let child = failing.try_submit().unwrap();
    assert_eq!(failing.admit_next(), Some(child));
    failing.complete(child, Err("fault")).unwrap();

    assert_eq!(parent.check_at(Instant::now()), Ok(()));
    assert!(unrelated.try_submit().is_ok());
    assert_eq!(
        failing.cancellation(),
        Some(&CancellationCause::ChildFailed(child))
    );
}
