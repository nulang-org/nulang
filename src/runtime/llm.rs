//! LLM subsystem for the actor runtime.
//!
//! Provider execution is isolated behind one process-wide bounded worker pool.
//! Runtime shards keep only their client/configuration and completion inbox;
//! each work item carries the originating shard's completion sender. This
//! avoids both the old single-worker bottleneck and a worker-pool-per-shard
//! explosion in multi-threaded nodes.

use std::sync::{Arc, OnceLock};

use nulang_ai::{LlmClient, LlmError, LlmRequest, LlmResponse, TokenBudget};

/// Default process-wide provider concurrency. Override with
/// `NULANG_LLM_WORKERS` (1..=64).
const DEFAULT_LLM_WORKERS: usize = 8;
const MAX_LLM_WORKERS: usize = 64;

/// Maximum number of provider requests waiting behind active workers.
/// Override with `NULANG_LLM_QUEUE_CAPACITY` (1..=65536).
const DEFAULT_LLM_QUEUE_CAPACITY: usize = 1024;
const MAX_LLM_QUEUE_CAPACITY: usize = 65_536;

type Completion = (u64, Result<LlmResponse, LlmError>);

fn env_bounded_usize(name: &str, default: usize, max: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| (1..=max).contains(value))
        .unwrap_or(default)
}

/// Work item submitted to the process-wide provider executor.
pub(crate) struct LlmWorkItem {
    pub(crate) actor_id: u64,
    pub(crate) request: LlmRequest,
    pub(crate) client: Arc<dyn LlmClient>,
    completion_tx: std::sync::mpsc::Sender<Completion>,
}

static LLM_EXECUTOR_TX: OnceLock<crossbeam::channel::Sender<LlmWorkItem>> = OnceLock::new();

/// Lazily initialize the process-wide bounded provider executor.
fn executor_tx() -> &'static crossbeam::channel::Sender<LlmWorkItem> {
    LLM_EXECUTOR_TX.get_or_init(|| {
        let worker_count =
            env_bounded_usize("NULANG_LLM_WORKERS", DEFAULT_LLM_WORKERS, MAX_LLM_WORKERS);
        let queue_capacity = env_bounded_usize(
            "NULANG_LLM_QUEUE_CAPACITY",
            DEFAULT_LLM_QUEUE_CAPACITY,
            MAX_LLM_QUEUE_CAPACITY,
        );
        let (request_tx, request_rx) = crossbeam::channel::bounded::<LlmWorkItem>(queue_capacity);

        for worker_id in 0..worker_count {
            let request_rx = request_rx.clone();
            let _worker = std::thread::Builder::new()
                .name(format!("nulang-llm-{worker_id}"))
                .spawn(move || {
                    let tokio_rt = match tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                    {
                        Ok(rt) => rt,
                        Err(_) => return,
                    };
                    while let Ok(item) = request_rx.recv() {
                        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            tokio_rt.block_on(item.client.complete(item.request))
                        }))
                        .unwrap_or_else(|_| {
                            Err(LlmError::from_string("LLM worker thread panicked"))
                        });
                        let _ = item.completion_tx.send((item.actor_id, result));
                    }
                });
        }

        // Only worker clones retain receivers after initialization. If every
        // worker fails to start or exits, try_send sees a disconnected channel
        // instead of accepting work that can never complete.
        drop(request_rx);
        request_tx
    })
}

/// Per-runtime-shard LLM state.
///
/// The expensive execution pool is process-wide; a shard owns only its
/// provider client, token budget, completion channel, and bookkeeping.
pub struct LlmState {
    /// Token budget for LLM calls. When set, the runtime rejects requests that
    /// cannot fit within the remaining estimated budget.
    pub token_budget: Option<Arc<TokenBudget>>,

    /// Provider client installed for this runtime shard.
    pub client: Option<Arc<dyn LlmClient>>,

    /// Completed provider calls destined for this runtime shard.
    pub rx: std::sync::mpsc::Receiver<Completion>,
    completion_tx: std::sync::mpsc::Sender<Completion>,

    /// Successfully dispatched calls whose completion has not yet been
    /// observed by the scheduler.
    pub inflight_count: usize,
}

impl LlmState {
    /// Create dormant per-shard state. This does not create provider workers;
    /// the process-wide executor starts on the first actual dispatch.
    pub fn new() -> Self {
        let (completion_tx, rx) = std::sync::mpsc::channel();
        LlmState {
            token_budget: None,
            client: None,
            rx,
            completion_tx,
            inflight_count: 0,
        }
    }

    pub fn set_client(&mut self, client: Box<dyn LlmClient>) {
        self.client = Some(Arc::from(client));
    }

    pub fn set_token_budget(&mut self, limit: u64) {
        self.token_budget = Some(Arc::new(TokenBudget::new(limit)));
    }

    pub fn clear_token_budget(&mut self) {
        self.token_budget = None;
    }

    pub fn check_token_budget(&self, estimated_tokens: u64) -> bool {
        self.token_budget
            .as_ref()
            .map(|budget| estimated_tokens <= budget.remaining())
            .unwrap_or(true)
    }

    pub fn record_token_usage(&self, tokens: u64) {
        if let Some(budget) = &self.token_budget {
            budget.charge(tokens);
        }
    }
}

impl Default for LlmState {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Request dispatch, completion polling, retry/fallback, and non-blocking
// suspension for `perform LLM.ask`.
// ---------------------------------------------------------------------------

use super::{
    agent, compute_backoff, suspension_marker, BytecodeDistributedCallbacks,
    BytecodeRuntimeCallbacks, Runtime,
};
use crate::runtime::persistence::WorkflowEvent;
use crate::vm::Value;

/// Drain completed background LLM calls and resume any actors waiting for
/// them.
pub(crate) fn poll_llm_completions(rt: &mut Runtime) {
    while let Ok((actor_id, result)) = rt.llm.rx.try_recv() {
        store_llm_completion(rt, actor_id, result);
    }
}

/// Record a completed background LLM call on its actor and resume the actor's
/// suspended behavior, if any.
pub(crate) fn store_llm_completion(
    rt: &mut Runtime,
    actor_id: u64,
    result: Result<LlmResponse, LlmError>,
) {
    rt.llm.inflight_count = rt.llm.inflight_count.saturating_sub(1);
    match result {
        Ok(response) => {
            if let Some(actor) = rt.actors.get_mut(&actor_id) {
                actor.llm_inflight = false;
                actor.llm_pending_prompt = None;
                actor.llm_completed = Some(Ok(response));
            }
            if rt
                .actors
                .get(&actor_id)
                .map(|a| a.suspended_execution.is_some())
                .unwrap_or(false)
            {
                resume_suspended_llm_step(rt, actor_id);
            }
        }
        Err(error) => handle_llm_error(rt, actor_id, error),
    }
}

/// Process an LLM error: decide whether to retry, fall back, or fail.
pub(crate) fn handle_llm_error(rt: &mut Runtime, actor_id: u64, error: LlmError) {
    let is_agent = rt
        .actors
        .get(&actor_id)
        .map(|a| a.is_agent)
        .unwrap_or(false);
    if !is_agent {
        if let Some(actor) = rt.actors.get_mut(&actor_id) {
            actor.llm_inflight = false;
            actor.llm_pending_prompt = None;
            actor.llm_completed = Some(Err(error));
            if actor.suspended_execution.is_some() {
                resume_suspended_llm_step(rt, actor_id);
                return;
            }
        }
        return;
    }

    let (retry_config, fallback_config, attempt, fallback_step, prompt) = {
        let actor = match rt.actors.get(&actor_id) {
            Some(a) => a,
            None => return,
        };
        let retry = actor.retry_config.clone();
        let fallback = actor.fallback_config.clone();
        let attempt_val = actor
            .get_state_field("llm_attempt")
            .and_then(|v| v.as_int())
            .unwrap_or(0) as u32;
        let fallback_step_val = actor
            .get_state_field("llm_fallback_step")
            .and_then(|v| v.as_int())
            .unwrap_or(0) as usize;
        let prompt_val = actor.llm_pending_prompt.clone().unwrap_or_default();
        (retry, fallback, attempt_val, fallback_step_val, prompt_val)
    };

    if let Some(retry) = &retry_config {
        if attempt < retry.max_attempts {
            let new_attempt = attempt + 1;
            if let Some(actor) = rt.actors.get_mut(&actor_id) {
                actor.llm_inflight = false;
                actor.set_state_field("llm_attempt", Value::int(new_attempt as i64));
            }
            let delay_ms = compute_backoff(retry, attempt, actor_id);
            rt.timer_wheel
                .schedule_llm_retry(std::time::Duration::from_millis(delay_ms), actor_id);
            return;
        }
    }

    if fallback_step < fallback_config.len() {
        let error_kind_name = format!("{:?}", error.kind);
        let fb = &fallback_config[fallback_step];
        let fb_matches = fb.on.is_empty() || fb.on.iter().any(|k| *k == error_kind_name);
        let new_fallback_step = fallback_step + 1;
        if fb_matches {
            if let Some(actor) = rt.actors.get_mut(&actor_id) {
                actor.llm_inflight = false;
                let model_ptr = actor.allocate_string(&fb.model);
                actor.set_state_field("model", model_ptr);
                actor.set_state_field("llm_attempt", Value::int(0));
                actor.set_state_field("llm_fallback_step", Value::int(new_fallback_step as i64));
                if let Some(max_tokens) = fb.max_tokens {
                    prune_episodic_memory(rt, actor_id, max_tokens);
                }
            }
            redispatch_llm_request(rt, actor_id, &prompt);
            return;
        }
        if let Some(actor) = rt.actors.get_mut(&actor_id) {
            actor.set_state_field("llm_attempt", Value::int(0));
            actor.set_state_field("llm_fallback_step", Value::int(new_fallback_step as i64));
        }
        handle_llm_error(rt, actor_id, error);
        return;
    }

    if let Some(actor) = rt.actors.get_mut(&actor_id) {
        actor.llm_inflight = false;
        actor.llm_pending_prompt = None;
        actor.llm_completed = Some(Err(error));
        if actor.suspended_execution.is_some() {
            resume_suspended_llm_step(rt, actor_id);
        }
    }
}

/// Re-dispatch an in-flight LLM request on retry timer fire.
pub(crate) fn handle_llm_retry_timer(rt: &mut Runtime, actor_id: u64) {
    let prompt = rt
        .actors
        .get(&actor_id)
        .and_then(|a| a.llm_pending_prompt.clone())
        .unwrap_or_default();
    if let Some(actor) = rt.actors.get_mut(&actor_id) {
        actor.llm_pending_prompt = None;
    }
    redispatch_llm_request(rt, actor_id, &prompt);
}

/// Build and dispatch an LLM request for the actor.
pub(crate) fn redispatch_llm_request(rt: &mut Runtime, actor_id: u64, prompt: &str) {
    let is_agent = rt
        .actors
        .get(&actor_id)
        .map(|a| a.is_agent)
        .unwrap_or(false);
    let request = if is_agent {
        agent::build_agent_llm_request(rt, actor_id, prompt)
    } else {
        let model = rt
            .actors
            .get(&actor_id)
            .and_then(|a| {
                let module = a.bytecode_module.as_ref()?;
                Runtime::vm_value_to_string(&a.get_state_field("model")?, Some(module))
            })
            .unwrap_or_default();
        rt.build_actor_llm_request(actor_id, &model, prompt)
    };
    let Some(request) = request else {
        if let Some(actor) = rt.actors.get_mut(&actor_id) {
            actor.llm_completed = Some(Ok(LlmResponse {
                content: None,
                tool_calls: Vec::new(),
                model: String::new(),
                finish_reason: "error".to_string(),
                usage: Default::default(),
            }));
            if actor.suspended_execution.is_some() {
                resume_suspended_llm_step(rt, actor_id);
                return;
            }
        }
        return;
    };
    if !dispatch_llm_request(rt, actor_id, request, prompt) {
        if let Some(actor) = rt.actors.get_mut(&actor_id) {
            actor.llm_inflight = false;
            actor.llm_pending_prompt = None;
            actor.llm_completed = Some(Ok(LlmResponse {
                content: None,
                tool_calls: Vec::new(),
                model: String::new(),
                finish_reason: "error".to_string(),
                usage: Default::default(),
            }));
            if actor.suspended_execution.is_some() {
                resume_suspended_llm_step(rt, actor_id);
            }
        }
    }
}

/// Try to enqueue an LLM request without blocking the actor scheduler.
///
/// Returns `false` on queue saturation or executor failure. In-flight state is
/// committed only after the bounded queue accepts the work item.
pub(crate) fn dispatch_llm_request(
    rt: &mut Runtime,
    actor_id: u64,
    request: LlmRequest,
    prompt: &str,
) -> bool {
    let Some(client) = rt.llm.client.clone() else {
        return false;
    };

    let item = LlmWorkItem {
        actor_id,
        request,
        client,
        completion_tx: rt.llm.completion_tx.clone(),
    };
    if executor_tx().try_send(item).is_err() {
        return false;
    }

    if let Some(actor) = rt.actors.get_mut(&actor_id) {
        actor.llm_inflight = true;
        actor.llm_pending_prompt = Some(prompt.to_string());
    }
    rt.llm.inflight_count += 1;
    true
}

/// Prune an agent's episodic memory to fit within `max_tokens`, using a
/// character-count heuristic (chars / 4).
pub(crate) fn prune_episodic_memory(rt: &mut Runtime, actor_id: u64, max_tokens: usize) {
    let memory_json = {
        let actor = match rt.actors.get(&actor_id) {
            Some(a) => a,
            None => return,
        };
        let module = match actor.bytecode_module.as_ref() {
            Some(m) => m,
            None => return,
        };
        Runtime::vm_value_to_string(
            &actor
                .get_state_field("episodic_memory")
                .unwrap_or(Value::nil()),
            Some(module),
        )
        .unwrap_or_default()
    };
    let mut memory: nulang_ai::EpisodicMemory =
        serde_json::from_str(&memory_json).unwrap_or_else(|_| nulang_ai::EpisodicMemory::new(50));

    let max_chars = max_tokens.saturating_mul(4);
    while memory.turns.iter().map(|t| t.content.len()).sum::<usize>() > max_chars
        && !memory.turns.is_empty()
    {
        if memory.turns.len() > 1 {
            memory.turns.remove(0);
        } else {
            break;
        }
    }

    let updated_json = serde_json::to_string(&memory).unwrap_or_default();
    if let Some(actor) = rt.actors.get_mut(&actor_id) {
        let ptr = actor.allocate_string(&updated_json);
        actor.set_state_field("episodic_memory", ptr);
    }
}

/// Resume an actor whose bytecode behavior suspended on an LLM provider call.
pub(crate) fn resume_suspended_llm_step(rt: &mut Runtime, actor_id: u64) {
    let suspended = match rt.actors.get_mut(&actor_id) {
        Some(actor) => actor.suspended_execution.take(),
        None => return,
    };
    let Some(suspended) = suspended else { return };

    if rt.vm.is_none() {
        if let Some(actor) = rt.actors.get_mut(&actor_id) {
            actor.suspended_execution = Some(suspended);
        }
        return;
    }

    let self_ptr: *mut Runtime = rt;
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
            Ok(_) => {
                if (*self_ptr).actor_is_workflow(actor_id) {
                    if let Some(actor) = (*self_ptr).actors.get_mut(&actor_id) {
                        actor.waiting_signal = None;
                        if let Some(n) =
                            actor.get_state_field("step_index").and_then(|v| v.as_int())
                        {
                            actor.set_state_field("step_index", Value::int(n + 1));
                        }
                    }
                    let seq = (*self_ptr).next_sequence(actor_id);
                    let _ = (*self_ptr).persistence.append_workflow_event(
                        actor_id,
                        WorkflowEvent::StepCompleted {
                            sequence: seq,
                            step_name: suspended.step_name,
                        },
                    );
                    (*self_ptr).checkpoint_actor(actor_id);
                }
            }
            Err(crate::types::NuError::Suspended(_)) => {
                if let Some(vm_state) = vm.take_suspended_state() {
                    let signal_name = vm.suspended_signal_name.take();
                    let receive_timeout = vm.suspended_receive_timeout.take();
                    if let Some(actor) = (*self_ptr).actors.get_mut(&actor_id) {
                        let marker = suspension_marker(actor, signal_name);
                        actor.waiting_signal = marker;
                        actor.suspended_execution =
                            Some(crate::runtime::actor::SuspendedExecution {
                                vm_state,
                                behavior_idx: suspended.behavior_idx,
                                step_name: suspended.step_name,
                            });
                    }
                    (*self_ptr).maybe_schedule_receive_wait(actor_id, receive_timeout);
                }
            }
            Err(_) => {}
        }
        (*self_ptr).vm_exec_end();
    }
    rt.requeue_if_mail_pending(actor_id);
}
