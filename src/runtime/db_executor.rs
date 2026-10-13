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
            Ok(vec![r#"[1,\"ok\"]"#.into()])
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
        assert_eq!(result.unwrap(), vec![r#"[1,\"ok\"]"#.to_string()]);
        let calls = calls.lock().unwrap();
        assert_eq!(calls[0].0, "SELECT $1");
        assert_ne!(calls[0].1, caller);
    }

    #[test]
    fn queue_backpressure_never_blocks_submitter() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let gate = Arc::new(Barrier::new(2));
        let client = Box::new(FakeClient {
            calls,
            gate: Some(gate.clone()),
        });
        let mut executor = BoundedDbQueryExecutor::new(vec![client], 1).unwrap();
        executor.try_submit(OwnedDbQuery::new("first", vec![])).unwrap();
        // The barrier must be reached before another item fills the queue.
        let started = Instant::now() + Duration::from_secs(2);
        while executor.inflight_count() == 0 && Instant::now() < started {
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
