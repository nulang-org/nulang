//! Native scoped-worker admission, cancellation and lexical join contract.
//! The host is opt-in and does not replace compiler-generated `par`.
#![cfg(not(target_arch = "wasm32"))]
use nulang::runtime::CancellationToken;
use nulang::scoped_tasks::{ScopeError, TaskId, TaskOutcome};
use nulang::scoped_thread_host::{
    run_scoped_threads, ScopedThreadError, ScopedThreadExit, ScopedThreadTask,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

#[test]
fn two_workers_really_overlap_and_join_before_their_borrowed_state_expires() {
    let borrowed = 9_u32;
    let (sender, receiver) = mpsc::channel::<()>();
    let tasks: Vec<ScopedThreadTask<'_, u32, &'static str>> = vec![
        Box::new(
            move |_| match receiver.recv_timeout(Duration::from_secs(3)) {
                Ok(()) => ScopedThreadExit::Completed(Ok(borrowed)),
                Err(_) => ScopedThreadExit::Completed(Err("second task did not run concurrently")),
            },
        ),
        Box::new(move |_| {
            sender.send(()).expect("first child still running");
            ScopedThreadExit::Completed(Ok(20))
        }),
    ];
    let result = run_scoped_threads(tasks, 2, 2, &CancellationToken::new()).unwrap();
    assert_eq!(
        result,
        vec![
            TaskOutcome::Completed(Ok(9)),
            TaskOutcome::Completed(Ok(20))
        ]
    );
}

#[test]
fn failure_cancels_pending_work_and_running_sibling_is_joined() {
    let (sender, receiver) = mpsc::channel();
    let queued_runs = Arc::new(AtomicUsize::new(0));
    let queued = queued_runs.clone();
    let tasks: Vec<ScopedThreadTask<'_, u32, &'static str>> = vec![
        Box::new(
            move |_| match receiver.recv_timeout(Duration::from_secs(3)) {
                Ok(()) => ScopedThreadExit::Completed(Err("first child failed")),
                Err(_) => ScopedThreadExit::Completed(Err("second child was not admitted")),
            },
        ),
        Box::new(move |cancel| {
            sender.send(()).unwrap();
            let end = Instant::now() + Duration::from_secs(3);
            while !cancel.is_cancelled() && Instant::now() < end {
                std::thread::yield_now();
            }
            if cancel.is_cancelled() {
                ScopedThreadExit::Cancelled
            } else {
                ScopedThreadExit::Completed(Err("sibling never observed cancellation"))
            }
        }),
        Box::new(move |_| {
            queued.fetch_add(1, Ordering::SeqCst);
            ScopedThreadExit::Completed(Ok(99))
        }),
    ];
    let result = run_scoped_threads(tasks, 2, 3, &CancellationToken::new()).unwrap();
    assert_eq!(
        result,
        vec![
            TaskOutcome::Completed(Err("first child failed")),
            TaskOutcome::Cancelled,
            TaskOutcome::Cancelled,
        ]
    );
    assert_eq!(queued_runs.load(Ordering::SeqCst), 0);
}

#[test]
fn a_panicked_child_signals_other_workers_and_does_not_escape_the_join() {
    let sibling_exited = Arc::new(AtomicUsize::new(0));
    let exited = sibling_exited.clone();
    let tasks: Vec<ScopedThreadTask<'_, u32, &'static str>> = vec![
        Box::new(|_| panic!("deliberate child panic")),
        Box::new(move |cancel| {
            let end = Instant::now() + Duration::from_secs(3);
            while !cancel.is_cancelled() && Instant::now() < end {
                std::thread::yield_now();
            }
            exited.fetch_add(1, Ordering::SeqCst);
            ScopedThreadExit::Cancelled
        }),
    ];
    let result = run_scoped_threads(tasks, 2, 2, &CancellationToken::new());
    assert!(matches!(
        result,
        Err(ScopedThreadError::ChildPanicked(TaskId(0)))
    ));
    assert_eq!(sibling_exited.load(Ordering::SeqCst), 1);
}

#[test]
fn queue_overflow_or_zero_limits_never_launch_workers() {
    let launched = Arc::new(AtomicUsize::new(0));
    let tasks = || -> Vec<ScopedThreadTask<'_, u32, &'static str>> {
        (0..2)
            .map(|_| {
                let launched = launched.clone();
                Box::new(move |_| {
                    launched.fetch_add(1, Ordering::SeqCst);
                    ScopedThreadExit::Completed(Ok(1_u32))
                }) as ScopedThreadTask<'_, u32, &'static str>
            })
            .collect()
    };
    let parent = CancellationToken::new();
    assert!(matches!(
        run_scoped_threads(tasks(), 0, 2, &parent),
        Err(ScopedThreadError::Scope(ScopeError::ZeroLimit))
    ));
    assert!(matches!(
        run_scoped_threads(tasks(), 1, 0, &parent),
        Err(ScopedThreadError::Scope(ScopeError::ZeroLimit))
    ));
    assert!(matches!(
        run_scoped_threads(tasks(), 1, 1, &parent),
        Err(ScopedThreadError::Scope(ScopeError::QueueFull))
    ));
    assert_eq!(launched.load(Ordering::SeqCst), 0);
}

#[test]
fn cancelled_parent_refuses_task_admission() {
    let parent = CancellationToken::new();
    parent.cancel();
    let task: ScopedThreadTask<'_, u32, &'static str> =
        Box::new(|_| panic!("must not run after parent cancellation"));
    assert!(matches!(
        run_scoped_threads(vec![task], 1, 1, &parent),
        Err(ScopedThreadError::Scope(ScopeError::Cancelled))
    ));
}

#[test]
fn parent_cancellation_during_execution_is_observed_before_join_returns() {
    let parent = CancellationToken::new();
    let cancel_parent = parent.clone();
    let (started, signal) = mpsc::channel();
    let external = std::thread::spawn(move || {
        signal
            .recv_timeout(Duration::from_secs(3))
            .expect("child started");
        cancel_parent.cancel();
    });
    let tasks: Vec<ScopedThreadTask<'_, u32, &'static str>> = vec![Box::new(move |token| {
        started.send(()).unwrap();
        let end = Instant::now() + Duration::from_secs(3);
        while !token.is_cancelled() && Instant::now() < end {
            std::thread::yield_now();
        }
        if token.is_cancelled() {
            ScopedThreadExit::Cancelled
        } else {
            ScopedThreadExit::Completed(Err("parent signal did not propagate"))
        }
    })];
    let outcome = run_scoped_threads(tasks, 1, 1, &parent).unwrap();
    external.join().unwrap();
    assert_eq!(outcome, vec![TaskOutcome::Cancelled]);
}
