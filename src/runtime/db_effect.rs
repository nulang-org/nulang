//! Actor-scoped asynchronous SQL integration.
//!
//! The tests intentionally precede the implementation. Unlike durable actor
//! effects, in-flight application SQL is not replay-safe.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{BlockingSubmitError, OwnedDbQueryClient};
    use std::sync::{Arc, Barrier, Mutex};
    use std::time::{Duration, Instant};

    #[test]
    fn db_completion_fence_rejects_wrong_activation_and_token() {
        let ticket = DbQueryTicket { worker: 0, job_id: 9 };
        let pending = PendingDbEffect {
            ticket,
            token: 12,
            activation_epoch: 3,
            completed: None,
        };
        assert!(pending.matches(ticket, 12, 3));
        assert!(!pending.matches(ticket, 13, 3));
        assert!(!pending.matches(ticket, 12, 4));
        assert!(!pending.matches(DbQueryTicket { worker: 0, job_id: 10 }, 12, 3));
    }

    #[test]
    fn owned_sql_args_reject_heap_pointers_and_bad_constant_ids() {
        let constants = vec![crate::bytecode::Constant::String("SELECT $1".into())];
        let valid = parse_query_args(
            &constants,
            &[Value::string(0), Value::int(7), Value::nil()],
        ).unwrap();
        assert_eq!(valid.sql, "SELECT $1");
        assert_eq!(valid.params, vec![OwnedDbParam::Int(7), OwnedDbParam::Null]);

        let invalid = parse_query_args(&constants, &[Value::string(3)]);
        assert!(invalid.is_err());
        // Heap pointers must never escape into a Send background job.
        let pointer = unsafe { Value::ptr(1usize as *mut u8) };
        let invalid = parse_query_args(&constants, &[Value::string(0), pointer]);
        assert!(invalid.is_err());
    }

    #[test]
    fn outstanding_query_never_dispatches_for_persistent_actors() {
        let mut rt = Runtime::new();
        let actor_id = rt.spawn_persistent_actor(Box::new(Vec::new()), Default::default());
        rt.suspend_enabled = true;
        let outcome = rt.start_or_resume_db_query(
            actor_id,
            &[crate::bytecode::Constant::String("SELECT 1".into())],
            &[Value::string(0)],
        );
        assert!(matches!(outcome, PerformAsyncResult::Ready(Some(_))));
        assert_eq!(rt.db_pending.len(), 0);
    }

    struct FakeClient {
        started: Arc<Barrier>,
        release: Arc<Barrier>,
        calls: Arc<Mutex<usize>>,
    }

    impl OwnedDbQueryClient for FakeClient {
        fn execute(&mut self, _: &OwnedDbQuery) -> io::Result<Vec<String>> {
            *self.calls.lock().unwrap() += 1;
            self.started.wait();
            self.release.wait();
            Ok(vec!["[42]".into()])
        }
    }

    #[test]
    fn polling_drops_deleted_actor_completions_without_replaying() {
        let started = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let calls = Arc::new(Mutex::new(0));
        let client = Box::new(FakeClient {
            started: started.clone(),
            release: release.clone(),
            calls: calls.clone(),
        });
        let mut rt = Runtime::new();
        rt.install_db_query_executor(BoundedDbQueryExecutor::new(vec![client], 2).unwrap())
            .unwrap();
        let id = rt.spawn_actor(Box::new(Vec::new()));
        rt.suspend_enabled = true;
        let first = rt.start_or_resume_db_query(
            id,
            &[crate::bytecode::Constant::String("SELECT 42".into())],
            &[Value::string(0)],
        );
        assert!(matches!(first, PerformAsyncResult::Pending));
        started.wait();

        rt.actors.remove(&id);
        release.wait();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !rt.db_pending.is_empty() && Instant::now() < deadline {
            rt.poll_db_query_completions();
            std::thread::yield_now();
        }
        assert!(rt.db_pending.is_empty());
        assert_eq!(*calls.lock().unwrap(), 1);
    }

    #[test]
    fn admission_without_pool_returns_an_explicit_error() {
        let mut rt = Runtime::new();
        let id = rt.spawn_actor(Box::new(Vec::new()));
        rt.suspend_enabled = true;
        let result = rt.start_or_resume_db_query(
            id,
            &[crate::bytecode::Constant::String("SELECT 1".into())],
            &[Value::string(0)],
        );
        assert!(matches!(result, PerformAsyncResult::Ready(Some(_))));
        assert!(rt.db_pending.is_empty());
    }
}
