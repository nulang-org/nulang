//! Reference executor for lexically scoped, cooperatively cancelled tasks.
//!
//! The source-level `par` construct is still sequential. This module defines
//! testable join/cancellation semantics for a future concurrent backend.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread;

/// Cooperative cancellation shared by one task scope and its descendants.
///
/// Cancellation does not interrupt blocking system calls or forcibly terminate
/// threads. Long-running children must check `is_cancelled` at safe points.
#[derive(Clone, Debug, Default)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
}

impl CancellationToken {
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

/// A child task may borrow from its lexical parent; it cannot escape the scope.
pub type ScopedTask<'a, T, E> = Box<dyn FnOnce(CancellationToken) -> Result<T, E> + Send + 'a>;

#[derive(Debug, PartialEq, Eq)]
pub enum ScopedTaskFailure<E> {
    Child { index: usize, error: E },
    Panicked { index: usize },
    Cancelled,
}

enum ChildOutcome<T, E> {
    Success(T),
    Failed(E),
    Panicked,
    Cancelled,
}

/// Reference implementation of a fail-fast, fully joined child-task scope.
///
/// Children run concurrently on OS threads (not Nulang actor scheduler
/// workers). The first child failure signals cancellation immediately; this
/// function still joins *all* children before returning. Successful results
/// remain in source order. This is a semantic reference for a future portable
/// `par` backend, **not** its current source-level execution path.
pub fn run_scoped_tasks<T: Send, E: Send>(
    parent: &CancellationToken,
    tasks: Vec<ScopedTask<'_, T, E>>,
) -> Result<Vec<T>, ScopedTaskFailure<E>> {
    if parent.is_cancelled() {
        return Err(ScopedTaskFailure::Cancelled);
    }

    let (results, first_failure) = thread::scope(|scope| {
        let handles: Vec<_> = tasks
            .into_iter()
            .map(|task| {
                let token = parent.clone();
                scope.spawn(move || {
                    if token.is_cancelled() {
                        return ChildOutcome::Cancelled;
                    }
                    match catch_unwind(AssertUnwindSafe(|| task(token.clone()))) {
                        Ok(Ok(value)) => ChildOutcome::Success(value),
                        Ok(Err(error)) => {
                            token.cancel();
                            ChildOutcome::Failed(error)
                        }
                        Err(_) => {
                            token.cancel();
                            ChildOutcome::Panicked
                        }
                    }
                })
            })
            .collect();

        let mut results = Vec::with_capacity(handles.len());
        let mut first_failure = None;
        // Joining in declaration order preserves result ordering; child
        // cancellation happens in the child thread, not in this loop.
        for (index, handle) in handles.into_iter().enumerate() {
            let outcome = handle.join().unwrap_or(ChildOutcome::Panicked);
            match outcome {
                ChildOutcome::Success(value) => results.push(value),
                ChildOutcome::Failed(error) => {
                    if first_failure.is_none() {
                        first_failure = Some(ScopedTaskFailure::Child { index, error });
                    }
                }
                ChildOutcome::Panicked => {
                    parent.cancel();
                    if first_failure.is_none() {
                        first_failure = Some(ScopedTaskFailure::Panicked { index });
                    }
                }
                ChildOutcome::Cancelled => {}
            }
        }

        (results, first_failure)
    });

    if let Some(failure) = first_failure {
        Err(failure)
    } else if parent.is_cancelled() {
        Err(ScopedTaskFailure::Cancelled)
    } else {
        Ok(results)
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
