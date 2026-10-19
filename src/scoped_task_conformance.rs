//! Test-only event trace conformance for the experimental scoped-task API.
//! Independent runtime backends can implement TraceHost to reuse these cases.

use crate::scoped_task_reference::{ReferenceExecutor, TaskPoll, TickStats};
use crate::scoped_tasks::{
    CancellationCause, CancellationToken, ScopeError, TaskId, TaskOutcome, TaskScope,
};

// This harness intentionally lives behind #[cfg(test)] in the main crate.
// It compares two *test hosts* of the same TaskScope contract, not the Nulang
// VM and WASM backends. Those can add adapters here once they support tasks.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Response {
    Pending,
    Success(u32),
    Failure(&'static str),
    Stop,
}

#[derive(Debug, Clone)]
enum TraceEvent {
    Submit,
    Tick(Vec<(usize, Response)>),
    Cancel(CancellationCause),
}

#[derive(Debug, PartialEq, Eq)]
struct StateSnapshot {
    running: usize,
    cancelled: Option<CancellationCause>,
    quiescent: bool,
}

#[derive(Debug, PartialEq, Eq)]
struct TraceRecord {
    submission: Option<Result<TaskId, ScopeError>>,
    tick: Option<TickStats>,
    state: StateSnapshot,
}

#[derive(Debug, PartialEq, Eq)]
struct TraceResult {
    records: Vec<TraceRecord>,
    outcomes: Vec<TaskOutcome<u32, &'static str>>,
}

/// A host adapts the same ordered event trace to its own scheduler. Future
/// bytecode/WASM hosts can implement this without copying the test fixtures.
trait TraceHost: Sized {
    fn try_submit(&mut self) -> Result<TaskId, ScopeError>;
    fn tick(&mut self, responses: &[(usize, Response)]) -> TickStats;
    fn cancel(&mut self, cause: CancellationCause);
    fn snapshot(&self) -> StateSnapshot;
    fn finish(self) -> Result<Vec<TaskOutcome<u32, &'static str>>, ScopeError>;
}

fn response_for(
    id: TaskId,
    responses: &[(usize, Response)],
    token: &CancellationToken,
) -> TaskPoll<u32, &'static str> {
    let response = responses
        .iter()
        .find(|(target, _)| *target == id.0)
        .map(|(_, response)| *response)
        .unwrap_or(Response::Pending);
    match response {
        Response::Pending => TaskPoll::Pending,
        Response::Success(value) => TaskPoll::Ready(Ok(value)),
        Response::Failure(error) => TaskPoll::Ready(Err(error)),
        // A child may only acknowledge cancellation when it has observed
        // the cooperative flag, never merely because a scripted event says so.
        Response::Stop if token.is_cancelled() => TaskPoll::Cancelled,
        Response::Stop => TaskPoll::Pending,
    }
}

impl TraceHost for ReferenceExecutor<u32, &'static str> {
    fn try_submit(&mut self) -> Result<TaskId, ScopeError> {
        ReferenceExecutor::try_submit(self)
    }

    fn tick(&mut self, responses: &[(usize, Response)]) -> TickStats {
        ReferenceExecutor::tick(self, |id, token| response_for(id, responses, token))
    }

    fn cancel(&mut self, cause: CancellationCause) {
        ReferenceExecutor::cancel(self, cause);
    }

    fn snapshot(&self) -> StateSnapshot {
        StateSnapshot {
            running: self.in_flight(),
            cancelled: self.cancellation().copied(),
            quiescent: self.is_quiescent(),
        }
    }

    fn finish(self) -> Result<Vec<TaskOutcome<u32, &'static str>>, ScopeError> {
        self.into_outcomes()
    }
}

/// An independent, intentionally simple test driver that calls TaskScope's
/// lifecycle API directly. It does not use ReferenceExecutor's tick loop.
struct DirectScopeHost {
    scope: TaskScope<u32, &'static str>,
    running: Vec<TaskId>,
}

impl DirectScopeHost {
    fn new(max_in_flight: usize, max_pending: usize) -> Self {
        Self {
            scope: TaskScope::new(max_in_flight, max_pending).expect("positive limits"),
            running: Vec::new(),
        }
    }
}

impl TraceHost for DirectScopeHost {
    fn try_submit(&mut self) -> Result<TaskId, ScopeError> {
        self.scope.try_submit()
    }

    fn cancel(&mut self, cause: CancellationCause) {
        self.scope.cancel(cause);
    }

    fn snapshot(&self) -> StateSnapshot {
        StateSnapshot {
            running: self.scope.in_flight(),
            cancelled: self.scope.cancellation().copied(),
            quiescent: self.scope.is_quiescent(),
        }
    }

    fn tick(&mut self, responses: &[(usize, Response)]) -> TickStats {
        let mut stats = TickStats::default();
        while let Some(id) = self.scope.admit_next() {
            self.running.push(id);
            stats.admitted += 1;
        }
        let token = self.scope.cancellation_token();
        // Iterate a snapshot so removal does not skip the next running task.
        for id in self.running.clone() {
            stats.polled += 1;
            match response_for(id, responses, &token) {
                TaskPoll::Pending => {}
                TaskPoll::Ready(result) => {
                    self.scope.complete(id, result).expect("running child");
                    self.running.retain(|active| *active != id);
                    stats.completed += 1;
                }
                TaskPoll::Cancelled => {
                    self.scope.acknowledge_cancelled(id).expect("running child");
                    self.running.retain(|active| *active != id);
                    stats.completed += 1;
                }
            }
        }
        stats
    }

    fn finish(self) -> Result<Vec<TaskOutcome<u32, &'static str>>, ScopeError> {
        self.scope.into_outcomes()
    }
}

fn run_trace<H: TraceHost>(mut host: H, events: &[TraceEvent]) -> TraceResult {
    let mut records = Vec::with_capacity(events.len());
    for event in events {
        let (submission, tick) = match event {
            TraceEvent::Submit => (Some(host.try_submit()), None),
            TraceEvent::Tick(responses) => (None, Some(host.tick(responses))),
            TraceEvent::Cancel(cause) => {
                host.cancel(*cause);
                (None, None)
            }
        };
        records.push(TraceRecord {
            submission,
            tick,
            state: host.snapshot(),
        });
    }
    TraceResult {
        records,
        outcomes: host
            .finish()
            .expect("fixture must end with no running work"),
    }
}

fn assert_hosts_conform(
    max_in_flight: usize,
    max_pending: usize,
    events: &[TraceEvent],
) -> TraceResult {
    let reference = run_trace(
        ReferenceExecutor::new(max_in_flight, max_pending).unwrap(),
        events,
    );
    let direct = run_trace(DirectScopeHost::new(max_in_flight, max_pending), events);
    assert_eq!(
        reference, direct,
        "reference and direct TaskScope hosts disagree on an ordered event trace"
    );
    reference
}

/// Vary admission pressure, completion order, and failure timing with a
/// reproducible seed. Explicit final cancellation plus stop acknowledgements
/// ensure every generated fixture closes before asserting the final results.
fn seeded_events(seed: u64) -> Vec<TraceEvent> {
    let mut rng = crate::dst::DeterministicRng::new(seed);
    let mut events = vec![
        TraceEvent::Submit,
        TraceEvent::Submit,
        TraceEvent::Tick(vec![(0, Response::Pending), (1, Response::Pending)]),
        TraceEvent::Submit,
    ];
    for _ in 0..32 {
        if rng.next() % 5 == 0 {
            events.push(TraceEvent::Submit);
        } else {
            let responses = (0..48)
                .map(|id| {
                    let answer = match rng.next() % 13 {
                        0 => Response::Failure("seeded failure"),
                        1..=4 => Response::Success(id as u32),
                        5 => Response::Stop,
                        _ => Response::Pending,
                    };
                    (id, answer)
                })
                .collect();
            events.push(TraceEvent::Tick(responses));
        }
    }
    events.push(TraceEvent::Cancel(CancellationCause::Parent));
    events.push(TraceEvent::Tick(
        (0..48).map(|id| (id, Response::Stop)).collect(),
    ));
    events
}

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
        assert_eq!(
            trace.records[2].submission,
            Some(Err(ScopeError::QueueFull))
        );
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
