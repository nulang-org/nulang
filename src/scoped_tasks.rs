//! Reference executor for lexically scoped, cooperatively cancelled tasks.
//!
//! The source-level `par` construct is still sequential. This module defines
//! testable join/cancellation semantics for a future concurrent backend.

use std::collections::VecDeque;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Mutex, OnceLock,
};
use std::thread;

pub use crate::task_cancellation::{CancellationReason, CancellationToken};

/// Maximum OS threads per reference task scope. Production scheduler
/// integration must use the actor/runtime's own resource budgets instead.
pub const MAX_SCOPED_WORKERS: usize = 64;

pub const MAX_SHARED_TASK_WORKERS: usize = 256;

/// Shared worker pool admission budget. Clone/Arc this across callers to
/// enforce a single limit across cooperating scopes; no blocking waits are
/// used because waiting inside a child scope could deadlock its parent.
#[derive(Debug)]
pub struct SharedTaskBudget {
    capacity: usize,
    active: AtomicUsize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SharedBudgetError {
    InvalidCapacity,
}

impl SharedTaskBudget {
    pub fn new(capacity: usize) -> Result<Self, SharedBudgetError> {
        if capacity == 0 || capacity > MAX_SHARED_TASK_WORKERS {
            return Err(SharedBudgetError::InvalidCapacity);
        }
        Ok(Self {
            capacity,
            active: AtomicUsize::new(0),
        })
    }

    pub fn active_workers(&self) -> usize {
        self.active.load(Ordering::Acquire)
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    fn try_reserve(&self, requested: usize) -> Option<WorkerReservation<'_>> {
        debug_assert!(requested > 0);
        loop {
            let active = self.active.load(Ordering::Acquire);
            let free = self.capacity.saturating_sub(active);
            if free == 0 {
                return None;
            }
            let count = requested.min(free);
            if self
                .active
                .compare_exchange_weak(active, active + count, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Some(WorkerReservation {
                    budget: self,
                    count,
                });
            }
        }
    }
}

/// RAII reservation: always release capacity after all started children join.
struct WorkerReservation<'a> {
    budget: &'a SharedTaskBudget,
    count: usize,
}

impl Drop for WorkerReservation<'_> {
    fn drop(&mut self) {
        self.budget.active.fetch_sub(self.count, Ordering::AcqRel);
    }
}

// All reference executor calls using run_scoped_tasks or run_scoped_tasks_bounded
// opt in to one process-wide limit. Native actors, VMs, and custom budgets
// are NOT accounted here until explicitly integrated.
static DEFAULT_SHARED_BUDGET: OnceLock<SharedTaskBudget> = OnceLock::new();

fn default_shared_budget() -> &'static SharedTaskBudget {
    DEFAULT_SHARED_BUDGET.get_or_init(|| {
        SharedTaskBudget::new(MAX_SCOPED_WORKERS)
            .expect("nonzero MAX_SCOPED_WORKERS must fit shared budget")
    })
}

/// A child task can borrow from its lexical parent; it cannot escape scope.
pub type ScopedTask<'a, T, E> = Box<dyn FnOnce(CancellationToken) -> Result<T, E> + Send + 'a>;

#[derive(Debug, PartialEq, Eq)]
pub enum ScopedTaskFailure<E> {
    Child { index: usize, error: E },
    Panicked { index: usize },
    WorkerPanicked,
    Cancelled,
    DeadlineExceeded,
    InvalidConcurrencyLimit,
    NoWorkerCapacity,
}

enum ChildOutcome<T, E> {
    Success(T),
    Failed(E),
    Panicked,
}

/// Default bounded reference executor. The minimum of two workers allows
/// overlapping I/O even on single-core hosts. Explicit callers may choose
/// another budget through `run_scoped_tasks_bounded`.
pub fn run_scoped_tasks<T: Send, E: Send>(
    parent: &CancellationToken,
    tasks: Vec<ScopedTask<'_, T, E>>,
) -> Result<Vec<T>, ScopedTaskFailure<E>> {
    let available = thread::available_parallelism().map_or(2, |n| n.get());
    let worker_budget = available.saturating_mul(2).clamp(2, 32);
    run_scoped_tasks_bounded(parent, tasks, worker_budget)
}

/// Concurrent reference implementation with a bounded number of OS threads.
///
/// Children are dequeued in source order and share a cancellation token.
/// Failures cancel siblings immediately, but every started child is joined
/// before returning. If multiple failures occur, report the smallest source
/// index among observed failures. Successful values retain source order.
///
/// This does not integrate with source-level `par` (still sequential), bytecode
/// VM, WASM, or the actor scheduler. Cancellation is always cooperative.
pub fn run_scoped_tasks_bounded<T: Send, E: Send>(
    parent: &CancellationToken,
    tasks: Vec<ScopedTask<'_, T, E>>,
    max_in_flight: usize,
) -> Result<Vec<T>, ScopedTaskFailure<E>> {
    run_scoped_tasks_with_budget(parent, tasks, max_in_flight, default_shared_budget())
}

/// Run a scope with a caller-supplied budget shared across other scopes.
///
/// Capacity is acquired atomically without waiting. If no worker slot is
/// available, return NoWorkerCapacity before executing any child, avoiding
/// nested-scope deadlocks. A partially available budget may reduce the actual
/// worker count. Permits are released after all started children are joined.
pub fn run_scoped_tasks_with_budget<T: Send, E: Send>(
    parent: &CancellationToken,
    tasks: Vec<ScopedTask<'_, T, E>>,
    max_in_flight: usize,
    budget: &SharedTaskBudget,
) -> Result<Vec<T>, ScopedTaskFailure<E>> {
    if max_in_flight == 0 || max_in_flight > MAX_SCOPED_WORKERS {
        return Err(ScopedTaskFailure::InvalidConcurrencyLimit);
    }
    if let Err(reason) = parent.checkpoint() {
        return Err(cancel_failure(reason));
    }
    if tasks.is_empty() {
        return Ok(vec![]);
    }
    let reservation = budget
        .try_reserve(max_in_flight.min(tasks.len()))
        .ok_or(ScopedTaskFailure::NoWorkerCapacity)?;
    run_scoped_tasks_bounded_inner(parent, tasks, reservation.count)
}

fn run_scoped_tasks_bounded_inner<T: Send, E: Send>(
    parent: &CancellationToken,
    tasks: Vec<ScopedTask<'_, T, E>>,
    max_in_flight: usize,
) -> Result<Vec<T>, ScopedTaskFailure<E>> {
    if max_in_flight == 0 || max_in_flight > MAX_SCOPED_WORKERS {
        return Err(ScopedTaskFailure::InvalidConcurrencyLimit);
    }
    if let Err(reason) = parent.checkpoint() {
        return Err(cancel_failure(reason));
    }
    if tasks.is_empty() {
        return Ok(vec![]);
    }

    let task_count = tasks.len();
    let worker_count = max_in_flight.min(task_count);
    let queue: Mutex<VecDeque<_>> = Mutex::new(tasks.into_iter().enumerate().collect());

    let (outcomes, worker_panicked) = thread::scope(|scope| {
        let mut workers = Vec::with_capacity(worker_count);
        for _ in 0..worker_count {
            let queue = &queue;
            let token = parent.clone();
            workers.push(scope.spawn(move || {
                let mut completed = Vec::new();
                while token.checkpoint().is_ok() {
                    // Never hold the queue lock while executing user code.
                    let next = queue
                        .lock()
                        .unwrap_or_else(|err| err.into_inner())
                        .pop_front();
                    let Some((index, task)) = next else {
                        break;
                    };
                    // Fail closed if another child cancelled after the pop.
                    if token.checkpoint().is_err() {
                        break;
                    }
                    let outcome = match catch_unwind(AssertUnwindSafe(|| task(token.clone()))) {
                        Ok(Ok(value)) => ChildOutcome::Success(value),
                        Ok(Err(error)) => {
                            token.cancel();
                            ChildOutcome::Failed(error)
                        }
                        Err(_) => {
                            token.cancel();
                            ChildOutcome::Panicked
                        }
                    };
                    completed.push((index, outcome));
                }
                completed
            }));
        }

        let mut outcomes = Vec::with_capacity(task_count);
        let mut worker_panicked = false;
        for worker in workers {
            match worker.join() {
                Ok(mut completed) => outcomes.append(&mut completed),
                Err(_) => {
                    parent.cancel();
                    worker_panicked = true;
                }
            }
        }
        (outcomes, worker_panicked)
    });

    let mut values: Vec<Option<T>> = std::iter::repeat_with(|| None).take(task_count).collect();
    let mut first_error: Option<(usize, ScopedTaskFailure<E>)> = None;
    for (index, outcome) in outcomes {
        match outcome {
            ChildOutcome::Success(value) => values[index] = Some(value),
            ChildOutcome::Failed(error) => {
                if first_error.as_ref().is_none_or(|(first, _)| index < *first) {
                    first_error = Some((index, ScopedTaskFailure::Child { index, error }));
                }
            }
            ChildOutcome::Panicked => {
                if first_error.as_ref().is_none_or(|(first, _)| index < *first) {
                    first_error = Some((index, ScopedTaskFailure::Panicked { index }));
                }
            }
        }
    }

    if let Some((_, failure)) = first_error {
        Err(failure)
    } else if worker_panicked {
        Err(ScopedTaskFailure::WorkerPanicked)
    } else if let Err(reason) = parent.checkpoint() {
        Err(cancel_failure(reason))
    } else {
        // With no cancellation or failure every child must have completed.
        Ok(values
            .into_iter()
            .map(|value| value.expect("active scope must have a result for every child"))
            .collect())
    }
}

fn cancel_failure<E>(reason: CancellationReason) -> ScopedTaskFailure<E> {
    match reason {
        CancellationReason::Cancelled => ScopedTaskFailure::Cancelled,
        CancellationReason::DeadlineExceeded => ScopedTaskFailure::DeadlineExceeded,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier};
    use std::time::{Duration, Instant};

    #[test]
    fn independent_children_overlap_but_results_keep_source_order() {
        let started = Arc::new(AtomicUsize::new(0));
        let tasks: Vec<ScopedTask<'_, usize, &'static str>> = (0usize..2)
            .map(|index| {
                let started = Arc::clone(&started);
                Box::new(move |_token| {
                    started.fetch_add(1, Ordering::SeqCst);
                    let began = Instant::now();
                    while started.load(Ordering::SeqCst) < 2
                        && began.elapsed() < Duration::from_secs(2)
                    {
                        std::thread::yield_now();
                    }
                    if started.load(Ordering::SeqCst) < 2 {
                        return Err("children did not overlap");
                    }
                    Ok(index)
                }) as ScopedTask<'_, usize, &'static str>
            })
            .collect();

        let values = run_scoped_tasks(&CancellationToken::default(), tasks).unwrap();
        assert_eq!(values, vec![0, 1]);
    }

    #[test]
    fn child_failure_cancels_siblings_and_joins_them_before_return() {
        let started = Arc::new(Barrier::new(2));
        let observed_cancel = Arc::new(AtomicBool::new(false));
        let tasks: Vec<ScopedTask<'_, usize, &'static str>> = vec![
            Box::new({
                let started = Arc::clone(&started);
                move |_token| {
                    started.wait();
                    Err("boom")
                }
            }),
            Box::new({
                let started = Arc::clone(&started);
                let observed_cancel = Arc::clone(&observed_cancel);
                move |token| {
                    started.wait();
                    let began = Instant::now();
                    while !token.is_cancelled() && began.elapsed() < Duration::from_secs(2) {
                        std::thread::yield_now();
                    }
                    observed_cancel.store(token.is_cancelled(), Ordering::SeqCst);
                    Ok(1)
                }
            }),
        ];
        let parent = CancellationToken::default();
        let result = run_scoped_tasks(&parent, tasks);
        assert!(matches!(
            result,
            Err(ScopedTaskFailure::Child {
                index: 0,
                error: "boom"
            })
        ));
        assert!(parent.is_cancelled());
        assert!(observed_cancel.load(Ordering::SeqCst));
    }

    #[test]
    fn already_cancelled_parent_does_not_start_children() {
        let parent = CancellationToken::default();
        parent.cancel();
        let started = AtomicBool::new(false);
        let tasks: Vec<ScopedTask<'_, usize, &'static str>> = vec![Box::new(|_| {
            started.store(true, Ordering::SeqCst);
            Ok(1)
        })];
        assert!(matches!(
            run_scoped_tasks(&parent, tasks),
            Err(ScopedTaskFailure::Cancelled)
        ));
        assert!(!started.load(Ordering::SeqCst));
    }

    #[test]
    fn panicking_child_cancels_scope_and_returns_a_typed_failure() {
        let parent = CancellationToken::default();
        let tasks: Vec<ScopedTask<'_, usize, &'static str>> =
            vec![Box::new(|_| panic!("intentional child panic"))];
        assert!(matches!(
            run_scoped_tasks(&parent, tasks),
            Err(ScopedTaskFailure::Panicked { index: 0 })
        ));
        assert!(parent.is_cancelled());
    }

    #[test]
    fn scoped_tasks_can_borrow_parent_data_without_static_lifetimes() {
        let text = String::from("nulang");
        let tasks: Vec<ScopedTask<'_, usize, &'static str>> = vec![Box::new(|_| Ok(text.len()))];
        assert_eq!(
            run_scoped_tasks(&CancellationToken::default(), tasks).unwrap(),
            vec![6]
        );
    }
}

#[cfg(test)]
mod bounded_tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier};
    use std::time::{Duration, Instant};

    #[test]
    fn bounded_workers_never_exceed_capacity_and_keep_source_order() {
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let tasks: Vec<ScopedTask<'_, usize, &'static str>> = (0usize..32)
            .map(|index| {
                let active = Arc::clone(&active);
                let peak = Arc::clone(&peak);
                Box::new(move |_| {
                    let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(current, Ordering::SeqCst);
                    thread::sleep(Duration::from_millis(4));
                    active.fetch_sub(1, Ordering::SeqCst);
                    Ok(index)
                }) as ScopedTask<'_, usize, &'static str>
            })
            .collect();
        let values = run_scoped_tasks_bounded(&CancellationToken::default(), tasks, 3).unwrap();
        assert_eq!(values, (0usize..32).collect::<Vec<_>>());
        assert!(peak.load(Ordering::SeqCst) >= 2);
        assert!(peak.load(Ordering::SeqCst) <= 3);
        assert_eq!(active.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn zero_capacity_rejected_before_any_task_runs() {
        let started = AtomicBool::new(false);
        let tasks: Vec<ScopedTask<'_, usize, &'static str>> = vec![Box::new(|_| {
            started.store(true, Ordering::SeqCst);
            Ok(1)
        })];
        assert!(matches!(
            run_scoped_tasks_bounded(&CancellationToken::default(), tasks, 0),
            Err(ScopedTaskFailure::InvalidConcurrencyLimit)
        ));
        assert!(!started.load(Ordering::SeqCst));
    }

    #[test]
    fn failure_cancels_queued_children_and_joins_workers() {
        let queued_executions = AtomicUsize::new(0);
        let mut tasks: Vec<ScopedTask<'_, usize, &'static str>> = vec![Box::new(|_| Err("boom"))];
        for _ in 0..20 {
            tasks.push(Box::new(|_| {
                queued_executions.fetch_add(1, Ordering::SeqCst);
                Ok(1)
            }));
        }
        let parent = CancellationToken::default();
        assert!(matches!(
            run_scoped_tasks_bounded(&parent, tasks, 1),
            Err(ScopedTaskFailure::Child {
                index: 0,
                error: "boom"
            })
        ));
        assert!(parent.is_cancelled());
        assert_eq!(queued_executions.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn concurrent_child_failures_have_deterministic_source_index() {
        let barrier = Arc::new(Barrier::new(2));
        let tasks: Vec<ScopedTask<'_, usize, &'static str>> = vec![
            Box::new({
                let barrier = Arc::clone(&barrier);
                move |_| {
                    barrier.wait();
                    thread::sleep(Duration::from_millis(8));
                    Err("first")
                }
            }),
            Box::new({
                let barrier = Arc::clone(&barrier);
                move |_| {
                    barrier.wait();
                    Err("second")
                }
            }),
        ];
        assert!(matches!(
            run_scoped_tasks_bounded(&CancellationToken::default(), tasks, 2),
            Err(ScopedTaskFailure::Child {
                index: 0,
                error: "first"
            })
        ));
    }

    #[test]
    fn expired_deadline_prevents_execution_and_is_distinguishable() {
        let token = CancellationToken::with_deadline(Instant::now() - Duration::from_millis(1));
        assert_eq!(
            token.checkpoint(),
            Err(CancellationReason::DeadlineExceeded)
        );
        let started = AtomicBool::new(false);
        let tasks: Vec<ScopedTask<'_, usize, &'static str>> = vec![Box::new(|_| {
            started.store(true, Ordering::SeqCst);
            Ok(42)
        })];
        assert!(matches!(
            run_scoped_tasks_bounded(&token, tasks, 2),
            Err(ScopedTaskFailure::DeadlineExceeded)
        ));
        assert!(!started.load(Ordering::SeqCst));
    }

    #[test]
    fn deadline_crossing_cancels_a_cooperative_child() {
        let deadline = Instant::now() + Duration::from_millis(15);
        let token = CancellationToken::with_deadline(deadline);
        let observed_expiry = AtomicBool::new(false);
        let tasks: Vec<ScopedTask<'_, usize, &'static str>> = vec![Box::new(|child| loop {
            if child.checkpoint() == Err(CancellationReason::DeadlineExceeded) {
                observed_expiry.store(true, Ordering::SeqCst);
                return Ok(1);
            }
            thread::sleep(Duration::from_millis(1));
        })];
        assert!(matches!(
            run_scoped_tasks_bounded(&token, tasks, 1),
            Err(ScopedTaskFailure::DeadlineExceeded)
        ));
        assert!(observed_expiry.load(Ordering::SeqCst));
    }

    #[test]
    fn empty_scope_returns_an_empty_collection() {
        let tasks: Vec<ScopedTask<'_, usize, &'static str>> = vec![];
        assert_eq!(
            run_scoped_tasks_bounded(&CancellationToken::default(), tasks, 2),
            Ok(vec![])
        );
    }
}

#[cfg(test)]
mod bounded_safety_tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    #[test]
    fn expired_deadline_wins_over_a_late_explicit_cancel() {
        let token = CancellationToken::with_deadline(Instant::now() - Duration::from_secs(1));
        token.cancel();
        assert_eq!(
            token.checkpoint(),
            Err(CancellationReason::DeadlineExceeded)
        );
    }

    #[test]
    fn capacity_above_global_per_scope_guard_is_rejected() {
        let started = AtomicBool::new(false);
        let tasks: Vec<ScopedTask<'_, usize, &'static str>> = vec![Box::new(|_| {
            started.store(true, Ordering::SeqCst);
            Ok(1)
        })];
        assert!(matches!(
            run_scoped_tasks_bounded(&CancellationToken::default(), tasks, MAX_SCOPED_WORKERS + 1),
            Err(ScopedTaskFailure::InvalidConcurrencyLimit)
        ));
        assert!(!started.load(Ordering::SeqCst));
    }

    #[test]
    fn explicit_cancellation_skips_queued_siblings() {
        let started = AtomicBool::new(false);
        let tasks: Vec<ScopedTask<'_, usize, &'static str>> = vec![
            Box::new(|token| {
                token.cancel();
                Ok(1)
            }),
            Box::new(|_| {
                started.store(true, Ordering::SeqCst);
                Ok(2)
            }),
        ];
        assert!(matches!(
            run_scoped_tasks_bounded(&CancellationToken::default(), tasks, 1),
            Err(ScopedTaskFailure::Cancelled)
        ));
        assert!(!started.load(Ordering::SeqCst));
    }
}

#[cfg(test)]
mod shared_budget_tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{mpsc, Arc};
    use std::time::Duration;

    #[test]
    fn shared_budget_denies_extra_scope_without_starting_queued_tasks() {
        let budget = Arc::new(SharedTaskBudget::new(1).unwrap());
        let parent = CancellationToken::default();
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let rejected_task_ran = AtomicBool::new(false);
        std::thread::scope(|scope| {
            let worker_budget = Arc::clone(&budget);
            let worker_parent = parent.clone();
            scope.spawn(move || {
                let tasks: Vec<ScopedTask<'_, usize, &'static str>> = vec![Box::new(move |_| {
                    started_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    Ok(1)
                })];
                assert_eq!(
                    run_scoped_tasks_with_budget(&worker_parent, tasks, 1, &worker_budget),
                    Ok(vec![1])
                );
            });
            started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            assert_eq!(budget.active_workers(), 1);
            let extra: Vec<ScopedTask<'_, usize, &'static str>> = vec![Box::new(|_| {
                rejected_task_ran.store(true, Ordering::SeqCst);
                Ok(2)
            })];
            assert_eq!(
                run_scoped_tasks_with_budget(&parent, extra, 1, &budget),
                Err(ScopedTaskFailure::NoWorkerCapacity)
            );
            assert!(!rejected_task_ran.load(Ordering::SeqCst));
            release_tx.send(()).unwrap();
        });
        assert_eq!(budget.active_workers(), 0);
    }

    #[test]
    fn nested_same_budget_fails_fast_instead_of_deadlocking() {
        let budget = SharedTaskBudget::new(1).unwrap();
        let parent = CancellationToken::default();
        let tasks: Vec<ScopedTask<'_, usize, &'static str>> = vec![Box::new(|token| {
            let nested: Vec<ScopedTask<'_, usize, &'static str>> = vec![Box::new(|_| Ok(99))];
            assert_eq!(
                run_scoped_tasks_with_budget(&token, nested, 1, &budget),
                Err(ScopedTaskFailure::NoWorkerCapacity)
            );
            Ok(7)
        })];
        assert_eq!(
            run_scoped_tasks_with_budget(&parent, tasks, 1, &budget),
            Ok(vec![7])
        );
        assert_eq!(budget.active_workers(), 0);
    }

    #[test]
    fn scope_failure_returns_permits_to_shared_budget() {
        let budget = SharedTaskBudget::new(2).unwrap();
        let parent = CancellationToken::default();
        let tasks: Vec<ScopedTask<'_, usize, &'static str>> = vec![Box::new(|_| Err("boom"))];
        assert_eq!(
            run_scoped_tasks_with_budget(&parent, tasks, 2, &budget),
            Err(ScopedTaskFailure::Child {
                index: 0,
                error: "boom"
            })
        );
        assert_eq!(budget.active_workers(), 0);
        let fresh = CancellationToken::default();
        let tasks: Vec<ScopedTask<'_, usize, &'static str>> = vec![Box::new(|_| Ok(42))];
        assert_eq!(
            run_scoped_tasks_with_budget(&fresh, tasks, 2, &budget),
            Ok(vec![42])
        );
    }

    #[test]
    fn shared_budget_rejects_zero_capacity_and_skips_pre_cancelled_scope() {
        assert!(SharedTaskBudget::new(0).is_err());
        let budget = SharedTaskBudget::new(2).unwrap();
        let parent = CancellationToken::default();
        parent.cancel();
        let ran = AtomicBool::new(false);
        let tasks: Vec<ScopedTask<'_, usize, &'static str>> = vec![Box::new(|_| {
            ran.store(true, Ordering::SeqCst);
            Ok(1)
        })];
        assert_eq!(
            run_scoped_tasks_with_budget(&parent, tasks, 1, &budget),
            Err(ScopedTaskFailure::Cancelled)
        );
        assert_eq!(budget.active_workers(), 0);
        assert!(!ran.load(Ordering::SeqCst));
    }
}
