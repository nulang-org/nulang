//! Activation-fenced async database query effect.
//!
//! Only ephemeral actors may use this initial API: durable actors need
//! idempotency receipts and transaction semantics before external SQL writes
//! may safely execute across a crash/replay boundary.

use std::io;

use super::{
    suspension_marker, BlockingSubmitError, BoundedDbQueryExecutor, BytecodeDistributedCallbacks,
    BytecodeRuntimeCallbacks, DbQueryOutcome, DbQueryTicket, OwnedDbParam, OwnedDbQuery, Runtime,
};
use crate::bytecode::Constant;
use crate::vm::{PerformAsyncResult, Value};

/// Scheduler-owned record for exactly one pending async query per actor.
pub(crate) struct PendingDbEffect {
    pub ticket: DbQueryTicket,
    pub token: u64,
    pub activation_epoch: u64,
    pub completed: Option<DbQueryOutcome>,
}

impl PendingDbEffect {
    fn matches(&self, ticket: DbQueryTicket, token: u64, activation_epoch: u64) -> bool {
        self.ticket == ticket && self.token == token && self.activation_epoch == activation_epoch
    }
}

/// Materialize arguments on the actor thread. Pointer-tagged VM values are
/// refused: their backing storage is not safe to dereference on SQL workers.
pub(crate) fn parse_query_args(
    constants: &[Constant],
    args: &[Value],
) -> io::Result<OwnedDbQuery> {
    const MAX_PARAMS: usize = 10;
    const MAX_SQL_BYTES: usize = 1_048_576;
    // PerformAsync passes all VM registers. r1 is a compiler-generated
    // argument count; never mistake uncleared registers for SQL parameters.
    let param_count = args
        .get(1)
        .and_then(Value::as_int)
        .filter(|count| (0..=MAX_PARAMS as i64).contains(count))
        .map(|count| count as usize)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid SQL arity"))?;
    if args.len() < param_count + 2 {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "SQL argument frame is truncated"));
    }
    let sql = args[0]
        .as_string_id()
        .and_then(|id| constants.get(id as usize))
        .and_then(|constant| match constant {
            Constant::String(text) => Some(text.clone()),
            _ => None,
        })
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "SQL must be a literal string"))?;
    if sql.trim().is_empty() || sql.len() > MAX_SQL_BYTES {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid SQL length"));
    }

    let mut params = Vec::with_capacity(param_count);
    for value in args.iter().skip(2).take(param_count) {
        let param = if value.is_nil() {
            OwnedDbParam::Null
        } else if let Some(i) = value.as_int() {
            OwnedDbParam::Int(i)
        } else if let Some(f) = value.as_float() {
            if !f.is_finite() {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "non-finite float"));
            }
            OwnedDbParam::Float(f)
        } else if let Some(b) = value.as_bool() {
            OwnedDbParam::Bool(b)
        } else if let Some(id) = value.as_string_id() {
            let text = constants
                .get(id as usize)
                .and_then(|c| match c {
                    Constant::String(text) => Some(text.clone()),
                    _ => None,
                })
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid string id"))?;
            OwnedDbParam::Text(text)
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unsupported SQL parameter; actor-owned pointers are forbidden",
            ));
        };
        params.push(param);
    }
    Ok(OwnedDbQuery::new(sql, params))
}

fn checked_error(kind: io::ErrorKind) -> PerformAsyncResult {
    let outcome = DbQueryOutcome::from_legacy_rows(Err(io::Error::from(kind)));
    PerformAsyncResult::Ready(Some(outcome.to_json()))
}

impl Runtime {
    /// Install a preconstructed bounded executor. Reconfiguration while a
    /// query is pending fails closed: completion ticket spaces cannot reset.
    pub fn install_db_query_executor(
        &mut self,
        executor: BoundedDbQueryExecutor,
    ) -> io::Result<()> {
        if !self.db_pending.is_empty() {
            return Err(io::Error::new(io::ErrorKind::WouldBlock, "database queries are pending"));
        }
        self.db_query_executor = Some(executor);
        Ok(())
    }

    /// Called from the PerformAsync VM callback; never blocks the scheduler.
    /// No database work is admitted for persistent actors until durable
    /// external-effect receipts and replay semantics have been specified.
    pub(crate) fn start_or_resume_db_query(
        &mut self,
        actor_id: u64,
        constants: &[Constant],
        args: &[Value],
    ) -> PerformAsyncResult {
        let Some(actor) = self.actors.get(&actor_id) else {
            return checked_error(io::ErrorKind::NotFound);
        };
        if actor.persistent || actor.is_workflow {
            return checked_error(io::ErrorKind::Unsupported);
        }

        if let Some(pending) = self.db_pending.get(&actor_id) {
            if actor.db_operation_id != Some(pending.token)
                || actor.activation_epoch != pending.activation_epoch
            {
                return checked_error(io::ErrorKind::PermissionDenied);
            }
            if let Some(outcome) = self.db_pending.remove(&actor_id) {
                if let Some(completed) = outcome.completed {
                    if let Some(actor) = self.actors.get_mut(&actor_id) {
                        actor.db_operation_id = None;
                    }
                    self.db_ticket_actors.remove(&outcome.ticket);
                    return PerformAsyncResult::Ready(Some(completed.to_json()));
                }
                // No completed result yet. Restore the in-flight entry.
                self.db_pending.insert(actor_id, outcome);
            }
            return PerformAsyncResult::Pending;
        }

        if !self.suspend_enabled {
            return checked_error(io::ErrorKind::Unsupported);
        }
        let request = match parse_query_args(constants, args) {
            Ok(request) => request,
            Err(error) => return checked_error(error.kind()),
        };
        let Some(next_token) = self.db_next_token.checked_add(1) else {
            return checked_error(io::ErrorKind::Other);
        };
        let Some(executor) = &mut self.db_query_executor else {
            return checked_error(io::ErrorKind::Unsupported);
        };
        let ticket = match executor.try_submit(request) {
            Ok(ticket) => ticket,
            Err(BlockingSubmitError::QueueFull) => return checked_error(io::ErrorKind::WouldBlock),
            Err(_) => return checked_error(io::ErrorKind::Other),
        };

        self.db_next_token = next_token;
        if let Some(actor) = self.actors.get_mut(&actor_id) {
            actor.db_operation_id = Some(next_token);
            self.db_pending.insert(
                actor_id,
                PendingDbEffect {
                    ticket,
                    token: next_token,
                    activation_epoch: actor.activation_epoch,
                    completed: None,
                },
            );
            self.db_ticket_actors.insert(ticket, actor_id);
            PerformAsyncResult::Pending
        } else {
            // Cannot revoke already-admitted SQL; the poller will discard
            // this completion because no actor owns the ticket.
            checked_error(io::ErrorKind::NotFound)
        }
    }

    /// Poll the worker completions without blocking, fence by actor instance
    /// token and activation epoch, and resume the matching VM suspension.
    pub(crate) fn poll_db_query_completions(&mut self) {
        if self.vm_execution_depth != 0 {
            return;
        }
        let ready = match self.db_query_executor.as_ref() {
            Some(executor) => executor.drain_ready(),
            None => return,
        };
        for completion in ready {
            let ticket = completion.ticket();
            let Some(actor_id) = self.db_ticket_actors.remove(&ticket) else {
                continue;
            };
            let valid = match (self.db_pending.get(&actor_id), self.actors.get(&actor_id)) {
                (Some(pending), Some(actor)) => {
                    actor.db_operation_id == Some(pending.token)
                        && pending.matches(ticket, pending.token, actor.activation_epoch)
                }
                _ => false,
            };
            if !valid {
                if let Some(stale) = self.db_pending.remove(&actor_id) {
                    if let Some(actor) = self.actors.get_mut(&actor_id) {
                        if actor.db_operation_id == Some(stale.token) {
                            actor.db_operation_id = None;
                        }
                    }
                }
                continue;
            }
            let (_, result) = completion.into_result();
            let outcome = DbQueryOutcome::from_legacy_rows(result);
            if let Some(pending) = self.db_pending.get_mut(&actor_id) {
                pending.completed = Some(outcome);
            }
        }

        // Do not resume an actor until its VM frames have been captured.
        // Completion may arrive while another actor owns the shared VM.
        let resumable: Vec<u64> = self.db_pending.iter().filter_map(|(&id, pending)| {
            let actor = self.actors.get(&id)?;
            (pending.completed.is_some()
                && actor.suspended_execution.is_some()
                && actor.db_operation_id == Some(pending.token)
                && actor.activation_epoch == pending.activation_epoch)
                .then_some(id)
        }).collect();
        for actor_id in resumable {
            self.resume_suspended_db_step(actor_id);
        }
    }

    /// Resume only the VM whose actor-level DB token matched at completion.
    /// The SQL result is consumed by re-executing the original PerformAsync.
    fn resume_suspended_db_step(&mut self, actor_id: u64) {
        let suspended = match self.actors.get_mut(&actor_id) {
            Some(actor) => actor.suspended_execution.take(),
            None => return,
        };
        let Some(suspended) = suspended else { return };
        if self.vm.is_none() {
            if let Some(actor) = self.actors.get_mut(&actor_id) {
                actor.suspended_execution = Some(suspended);
            }
            return;
        }
        self.restore_suspended_workflow_activation(actor_id, suspended.activation);
        let self_ptr: *mut Runtime = self;
        unsafe {
            let vm = (*self_ptr).vm.as_mut().unwrap();
            vm.set_actor_callbacks(Box::new(BytecodeRuntimeCallbacks::new(self_ptr, actor_id)));
            vm.set_distributed_callbacks(Box::new(BytecodeDistributedCallbacks { runtime: self_ptr }));
            vm.restore_suspended_state(suspended.vm_state);
            let saved_suspend = (*self_ptr).suspend_enabled;
            (*self_ptr).suspend_enabled = true;
            (*self_ptr).vm_exec_begin();
            let result = vm.resume();
            (*self_ptr).suspend_enabled = saved_suspend;
            match result {
                Ok(_) => {}
                Err(crate::types::NuError::Suspended(_)) => {
                    if let Some(vm_state) = vm.take_suspended_state() {
                        let signal_name = vm.suspended_signal_name.take();
                        let receive_timeout = vm.suspended_receive_timeout.take();
                        if let Some(actor) = (*self_ptr).actors.get_mut(&actor_id) {
                            actor.waiting_signal = suspension_marker(actor, signal_name);
                            actor.suspended_execution = Some(super::actor::SuspendedExecution {
                                vm_state,
                                behavior_idx: suspended.behavior_idx,
                                activation: suspended.activation,
                                step_name: suspended.step_name,
                            });
                        }
                        (*self_ptr).maybe_schedule_receive_wait(actor_id, receive_timeout);
                    }
                }
                Err(error) => {
                    tracing::warn!(actor_id, %error, "database query VM resume failed");
                }
            }
            (*self_ptr).vm_exec_end();
        }
        self.requeue_if_mail_pending(actor_id);
    }
}

//! Actor-scoped asynchronous SQL integration.
//!
//! The tests intentionally precede the implementation. Unlike durable actor
//! effects, in-flight application SQL is not replay-safe.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::OwnedDbQueryClient;
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
            &[Value::string(0), Value::int(2), Value::int(7), Value::nil()],
        ).unwrap();
        assert_eq!(valid.sql, "SELECT $1");
        assert_eq!(valid.params, vec![OwnedDbParam::Int(7), OwnedDbParam::Null]);

        let invalid = parse_query_args(&constants, &[Value::string(3), Value::int(0)]);
        assert!(invalid.is_err());
        // Heap pointers must never escape into a Send background job.
        let pointer = unsafe { Value::ptr(1usize as *mut u8) };
        let invalid = parse_query_args(&constants, &[Value::string(0), Value::int(1), pointer]);
        assert!(invalid.is_err());
    }

    #[test]
    fn sql_arity_ignores_unused_registers_and_rejects_invalid_count() {
        let constants = [Constant::String("SELECT $1".into())];
        let query = parse_query_args(
            &constants,
            &[Value::string(0), Value::int(1), Value::int(42), Value::int(999)],
        )
        .unwrap();
        assert_eq!(query.params, vec![OwnedDbParam::Int(42)]);
        assert!(parse_query_args(&constants, &[Value::string(0), Value::int(-1)]).is_err());
        assert!(parse_query_args(&constants, &[Value::string(0), Value::int(255)]).is_err());
        assert!(parse_query_args(
            &constants,
            &[Value::string(0), Value::int(2), Value::int(42)],
        )
        .is_err());
    }

    #[test]
    fn outstanding_query_never_dispatches_for_persistent_actors() {
        let mut rt = Runtime::new();
        let actor_id = rt.spawn_persistent_actor(Box::new(Vec::new()), Default::default());
        rt.suspend_enabled = true;
        let outcome = rt.start_or_resume_db_query(
            actor_id,
            &[crate::bytecode::Constant::String("SELECT 1".into())],
            &[Value::string(0), Value::int(0)],
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
            &[Value::string(0), Value::int(0)],
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
    fn completed_query_is_consumed_exactly_once_and_preserves_rows() {
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
        let constants = [Constant::String("SELECT 42".into())];
        let args = [Value::string(0), Value::int(0)];
        assert!(matches!(
            rt.start_or_resume_db_query(id, &constants, &args),
            PerformAsyncResult::Pending
        ));
        started.wait();
        release.wait();
        let deadline = Instant::now() + Duration::from_secs(2);
        while rt.db_pending.get(&id).is_some_and(|p| p.completed.is_none())
            && Instant::now() < deadline
        {
            rt.poll_db_query_completions();
            std::thread::yield_now();
        }
        let completed = rt.start_or_resume_db_query(id, &constants, &args);
        match completed {
            PerformAsyncResult::Ready(Some(encoded)) => {
                assert_eq!(encoded, r#"{"status":"ok","rows":[[42]]}"#);
            }
            other => panic!("unexpected SQL result: {other:?}"),
        }
        assert!(!rt.db_pending.contains_key(&id));
        assert_eq!(rt.actors[&id].db_operation_id, None);
        assert_eq!(*calls.lock().unwrap(), 1);
    }

    #[test]
    fn changed_activation_epoch_discards_old_sql_completion() {
        let started = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let client = Box::new(FakeClient {
            started: started.clone(),
            release: release.clone(),
            calls: Arc::new(Mutex::new(0)),
        });
        let mut rt = Runtime::new();
        rt.install_db_query_executor(BoundedDbQueryExecutor::new(vec![client], 2).unwrap())
            .unwrap();
        let id = rt.spawn_actor(Box::new(Vec::new()));
        rt.suspend_enabled = true;
        assert!(matches!(
            rt.start_or_resume_db_query(
                id,
                &[Constant::String("SELECT 42".into())],
                &[Value::string(0), Value::int(0)]
            ),
            PerformAsyncResult::Pending
        ));
        started.wait();
        rt.actors.get_mut(&id).unwrap().activation_epoch += 1;
        release.wait();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !rt.db_pending.is_empty() && Instant::now() < deadline {
            rt.poll_db_query_completions();
            std::thread::yield_now();
        }
        assert!(rt.db_pending.is_empty());
        assert!(rt.db_ticket_actors.is_empty());
        assert_eq!(rt.actors[&id].db_operation_id, None);
    }

    #[test]
    fn admission_without_pool_returns_an_explicit_error() {
        let mut rt = Runtime::new();
        let id = rt.spawn_actor(Box::new(Vec::new()));
        rt.suspend_enabled = true;
        let result = rt.start_or_resume_db_query(
            id,
            &[crate::bytecode::Constant::String("SELECT 1".into())],
            &[Value::string(0), Value::int(0)],
        );
        assert!(matches!(result, PerformAsyncResult::Ready(Some(_))));
        assert!(rt.db_pending.is_empty());
    }
}
