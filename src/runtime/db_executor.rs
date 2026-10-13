//! Bounded worker-owned SQL execution, independent of actor durability.
//!
//! This adapter does not execute on the cooperative scheduler. Workers receive
//! owned SQL and parameter values only; never raw actor-heap pointers.
//! Actor suspension and cancellation wiring are separate follow-up work.

use crossbeam::channel::TryRecvError;
use std::io;
use std::sync::{Arc, Mutex};

use super::{
    BlockingCompletion, BlockingExecutor, BlockingExecutorConfigError, BlockingJobId,
    BlockingSubmitError,
};

/// Values safe to move to a database worker thread.
#[derive(Debug, Clone, PartialEq)]
pub enum OwnedDbParam {
    Int(i64),
    Float(f64),
    Bool(bool),
    Text(String),
    Json(serde_json::Value),
    Null,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OwnedDbQuery {
    pub sql: String,
    pub params: Vec<OwnedDbParam>,
}

impl OwnedDbQuery {
    pub fn new(sql: impl Into<String>, params: Vec<OwnedDbParam>) -> Self {
        Self { sql: sql.into(), params }
    }
}

/// Client instances are worker-owned. A caller can supply its own TLS-enabled
/// PostgreSQL connection, or another backend, without exposing VM values.
pub trait OwnedDbQueryClient: Send {
    fn execute(&mut self, request: &OwnedDbQuery) -> io::Result<Vec<String>>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DbQueryTicket {
    pub worker: usize,
    pub job_id: BlockingJobId,
}

#[derive(Debug)]
pub enum DbQueryCompletion {
    Finished {
        ticket: DbQueryTicket,
        result: io::Result<Vec<String>>,
    },
    Panicked {
        ticket: DbQueryTicket,
    },
}

impl DbQueryCompletion {
    pub fn ticket(&self) -> DbQueryTicket {
        match self {
            Self::Finished { ticket, .. } | Self::Panicked { ticket } => *ticket,
        }
    }

    /// Always produce an explicit error, including for a worker panic.
    pub fn into_result(self) -> (DbQueryTicket, io::Result<Vec<String>>) {
        match self {
            Self::Finished { ticket, result } => (ticket, result),
            Self::Panicked { ticket } => (
                ticket,
                Err(io::Error::other("database query worker panicked")),
            ),
        }
    }
}

struct DbWorker {
    executor: BlockingExecutor<io::Result<Vec<String>>>,
    client: Arc<Mutex<Box<dyn OwnedDbQueryClient>>>,
}

/// One worker thread per supplied database connection, with bounded queues.
///
/// Each worker preserves its connection ownership and serializes access.
/// This avoids holding any database mutex on an actor scheduler thread.
/// Completion polling is nonblocking; callers must poll often enough to
/// avoid completed-result backpressure.
pub struct BoundedDbQueryExecutor {
    workers: Vec<DbWorker>,
    next_worker: usize,
}

impl BoundedDbQueryExecutor {
    pub fn new(
        clients: Vec<Box<dyn OwnedDbQueryClient>>,
        queue_capacity_per_worker: usize,
    ) -> Result<Self, BlockingExecutorConfigError> {
        if clients.is_empty() {
            return Err(BlockingExecutorConfigError::ZeroWorkers);
        }
        if queue_capacity_per_worker == 0 {
            return Err(BlockingExecutorConfigError::ZeroQueueCapacity);
        }
        let mut workers = Vec::with_capacity(clients.len());
        for client in clients {
            workers.push(DbWorker {
                executor: BlockingExecutor::new(1, queue_capacity_per_worker)?,
                client: Arc::new(Mutex::new(client)),
            });
        }
        Ok(Self { workers, next_worker: 0 })
    }

    /// Admit a query without waiting for a connection, a network operation,
    /// or a worker. Backpressure is explicit when all queues are full.
    pub fn try_submit(
        &mut self,
        request: OwnedDbQuery,
    ) -> Result<DbQueryTicket, BlockingSubmitError> {
        let request = Arc::new(request);
        let mut closed = 0;
        for offset in 0..self.workers.len() {
            let index = (self.next_worker + offset) % self.workers.len();
            let client = Arc::clone(&self.workers[index].client);
            let request = Arc::clone(&request);
            match self.workers[index].executor.try_submit(move || {
                let mut client = client.lock().map_err(|_| {
                    io::Error::other("database worker connection mutex poisoned")
                })?;
                client.execute(&request)
            }) {
                Ok(job_id) => {
                    self.next_worker = (index + 1) % self.workers.len();
                    return Ok(DbQueryTicket { worker: index, job_id });
                }
                Err(BlockingSubmitError::QueueFull) => {}
                Err(BlockingSubmitError::Closed) => closed += 1,
                Err(BlockingSubmitError::IdExhausted) => {
                    return Err(BlockingSubmitError::IdExhausted);
                }
            }
        }
        if closed == self.workers.len() {
            Err(BlockingSubmitError::Closed)
        } else {
            Err(BlockingSubmitError::QueueFull)
        }
    }

    pub fn drain_ready(&self) -> Vec<DbQueryCompletion> {
        let mut ready = Vec::new();
        for (worker, slot) in self.workers.iter().enumerate() {
            loop {
                match slot.executor.try_recv() {
                    Ok(BlockingCompletion::Finished { id, value }) => {
                        ready.push(DbQueryCompletion::Finished {
                            ticket: DbQueryTicket { worker, job_id: id },
                            result: value,
                        });
                    }
                    Ok(BlockingCompletion::Panicked { id }) => {
                        ready.push(DbQueryCompletion::Panicked {
                            ticket: DbQueryTicket { worker, job_id: id },
                        });
                    }
                    Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
                }
            }
        }
        ready
    }

    pub fn worker_count(&self) -> usize {
        self.workers.len()
    }

    pub fn close(&mut self) {
        for worker in &mut self.workers {
            worker.executor.close();
        }
    }
}

//! Regression tests for the bounded, worker-owned database execution boundary.
//! The implementation intentionally works with fully-owned requests rather
//! than raw VM Values, which can contain actor-heap pointers.

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier, Mutex};
    use std::time::{Duration, Instant};

    #[derive(Debug)]
    struct FakeClient {
        calls: Arc<Mutex<Vec<(String, std::thread::ThreadId)>>>,
        gate: Option<Arc<Barrier>>,
    }

    impl OwnedDbQueryClient for FakeClient {
        fn execute(&mut self, request: &OwnedDbQuery) -> io::Result<Vec<String>> {
            self.calls
                .lock()
                .unwrap()
                .push((request.sql.clone(), std::thread::current().id()));
            if let Some(gate) = self.gate.take() {
                gate.wait();
            }
            Ok(vec![r#"[1,"ok"]"#.into()])
        }
    }

    fn receive(
        executor: &BoundedDbQueryExecutor,
        deadline: Instant,
    ) -> DbQueryCompletion {
        loop {
            if let Some(result) = executor.drain_ready().into_iter().next() {
                return result;
            }
            if Instant::now() >= deadline {
                panic!("database completion did not arrive before the deadline");
            }
            std::thread::yield_now();
        }
    }

    #[test]
    fn owned_query_executes_off_caller_thread() {
        let caller = std::thread::current().id();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let client = Box::new(FakeClient {
            calls: calls.clone(),
            gate: None,
        });
        let mut executor = BoundedDbQueryExecutor::new(vec![client], 2).unwrap();
        let ticket = executor
            .try_submit(OwnedDbQuery::new("SELECT $1", vec![OwnedDbParam::Int(1)]))
            .unwrap();
        let completion = receive(&executor, Instant::now() + Duration::from_secs(2));
        assert_eq!(completion.ticket(), ticket);
        let (_, result) = completion.into_result();
        assert_eq!(result.unwrap(), vec![r#"[1,"ok"]"#.to_string()]);
        let calls = calls.lock().unwrap();
        assert_eq!(calls[0].0, "SELECT $1");
        assert_ne!(calls[0].1, caller);
    }

    #[test]
    fn queue_backpressure_never_blocks_submitter() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let gate = Arc::new(Barrier::new(2));
        let client = Box::new(FakeClient {
            calls: calls.clone(),
            gate: Some(gate.clone()),
        });
        let mut executor = BoundedDbQueryExecutor::new(vec![client], 1).unwrap();
        executor.try_submit(OwnedDbQuery::new("first", vec![])).unwrap();
        // The barrier must be reached before another item fills the queue.
        let started = Instant::now() + Duration::from_secs(2);
        while calls.lock().unwrap().is_empty() && Instant::now() < started {
            std::thread::yield_now();
        }
        executor.try_submit(OwnedDbQuery::new("second", vec![])).unwrap();
        assert_eq!(
            executor.try_submit(OwnedDbQuery::new("third", vec![])),
            Err(BlockingSubmitError::QueueFull)
        );
        gate.wait();
        executor.close();
    }

    #[test]
    fn rejects_empty_worker_pool_and_queue() {
        assert!(matches!(
            BoundedDbQueryExecutor::new(vec![], 1),
            Err(BlockingExecutorConfigError::ZeroWorkers)
        ));
        assert!(matches!(
            BoundedDbQueryExecutor::new(
                vec![Box::new(FakeClient {
                    calls: Arc::new(Mutex::new(Vec::new())),
                    gate: None,
                })],
                0,
            ),
            Err(BlockingExecutorConfigError::ZeroQueueCapacity)
        ));
    }

    #[test]
    fn completed_errors_keep_ticket_and_produce_checked_envelope() {
        struct FailingClient;
        impl OwnedDbQueryClient for FailingClient {
            fn execute(&mut self, _: &OwnedDbQuery) -> io::Result<Vec<String>> {
                Err(io::Error::new(io::ErrorKind::Unsupported, "private details"))
            }
        }
        let mut executor = BoundedDbQueryExecutor::new(vec![Box::new(FailingClient)], 2).unwrap();
        let ticket = executor.try_submit(OwnedDbQuery::new("SELECT 1", vec![])).unwrap();
        let completion = receive(&executor, Instant::now() + Duration::from_secs(2));
        let (actual_ticket, result) = completion.into_result();
        assert_eq!(actual_ticket, ticket);
        let checked = super::super::DbQueryOutcome::from_legacy_rows(result).to_json();
        assert!(checked.contains(r#""code":"unsupported""#));
        assert!(!checked.contains("private details"));
    }

    #[test]
    fn two_clients_have_distinct_worker_tickets() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let clients: Vec<Box<dyn OwnedDbQueryClient>> = (0..2)
            .map(|_| {
                Box::new(FakeClient {
                    calls: calls.clone(),
                    gate: None,
                }) as Box<dyn OwnedDbQueryClient>
            })
            .collect();
        let mut executor = BoundedDbQueryExecutor::new(clients, 2).unwrap();
        let first = executor.try_submit(OwnedDbQuery::new("first", vec![])).unwrap();
        let second = executor.try_submit(OwnedDbQuery::new("second", vec![])).unwrap();
        assert_ne!(first.worker, second.worker);
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut done = Vec::new();
        while done.len() < 2 && Instant::now() < deadline {
            done.extend(executor.drain_ready().into_iter().map(|c| c.ticket()));
            std::thread::yield_now();
        }
        assert!(done.contains(&first));
        assert!(done.contains(&second));
    }
}
