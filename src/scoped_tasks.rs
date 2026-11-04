//! Reference executor for lexically scoped, cooperatively cancelled tasks.
//!
//! The source-level `par` construct is still sequential. This module defines
//! testable join/cancellation semantics for a future concurrent backend.

use std::collections::VecDeque;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{
    atomic::{AtomicU8, Ordering},
    Arc, Mutex,
};
use std::thread;
use std::time::Instant;

/// Maximum OS threads per reference task scope. Production scheduler
/// integration must use the actor/runtime's own resource budgets instead.
pub const MAX_SCOPED_WORKERS: usize = 64;
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
