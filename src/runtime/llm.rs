//! LLM subsystem for the actor runtime.
//!
//! Manages the persistent LLM worker thread, request dispatch, completion
//! polling, and non-blocking suspension for `perform LLM.ask` in bytecode
//! behaviors.

use std::collections::HashMap;
use std::sync::Arc;

const LLM_REQUEST_QUEUE_CAPACITY: usize = 64;
const MAX_CONCURRENT_LLM_REQUESTS: usize = 16;

use nulang_ai::{LlmClient, LlmError, LlmRequest, LlmResponse, TokenBudget};

/// Work item sent to the persistent LLM worker thread.
pub(crate) struct LlmWorkItem {
    pub(crate) actor_id: u64,
    pub(crate) request: LlmRequest,
    pub(crate) client: Arc<dyn LlmClient>,
}

/// Consolidated LLM subsystem state.
///
/// Extracted from the Runtime god-object to group related fields and
/// clarify ownership. The worker thread is spawned in [`LlmState::new`]
/// and runs for the lifetime of the runtime.
pub struct LlmState {
    /// Token budget for LLM calls. When set, the runtime rejects
    /// LLM requests that would exceed the configured token limit.
    pub token_budget: Option<Arc<TokenBudget>>,

    /// LLM client for the v0.9 AI Runtime. Shared (Arc) so background worker
    /// threads can perform non-blocking `perform LLM.ask` calls.
    pub client: Option<Arc<dyn LlmClient>>,

    /// Channel receiving results from the persistent LLM worker thread.
    /// Drained by `poll_llm_completions`.
    pub rx: std::sync::mpsc::Receiver<(u64, Result<LlmResponse, LlmError>)>,

    /// Number of LLM calls currently in flight. Incremented on dispatch,
    /// decremented when the completion is stored.
    pub inflight_count: usize,

    /// Bounded, nonblocking request admission; the Tokio worker owns the receiver.
    pub(crate) request_tx: Option<tokio::sync::mpsc::Sender<LlmWorkItem>>,
}

impl LlmState {
    /// Create one Tokio worker thread per runtime shard. Each worker overlaps
    /// up to MAX_CONCURRENT_LLM_REQUESTS async provider calls; mutable actor
    /// state and VM callbacks remain entirely on the shard scheduler thread.
    pub fn new() -> Self {
        let (llm_tx, llm_rx) = std::sync::mpsc::channel();
        let (llm_request_tx, llm_request_rx) =
            tokio::sync::mpsc::channel::<LlmWorkItem>(LLM_REQUEST_QUEUE_CAPACITY);

        // An async mpsc receiver is essential here: a blocking recv() on the
        // Tokio thread would prevent already-started futures from progressing.
        let _worker = std::thread::Builder::new()
            .name("nulang-llm".to_string())
            .spawn(move || {
                let tokio_rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(_) => return,
                };
                tokio_rt.block_on(run_llm_worker(llm_request_rx, llm_tx));
            });

        LlmState {
            token_budget: None,
            client: None,
            rx: llm_rx,
            inflight_count: 0,
            request_tx: Some(llm_request_tx),
        }
    }
}

/// Execute asynchronous requests concurrently on the worker's Tokio thread.
/// A task panic must still produce a completion for its actor, otherwise the
/// scheduler's inflight count would never drain and suspension could hang.
async fn run_llm_worker(
    mut requests: tokio::sync::mpsc::Receiver<LlmWorkItem>,
    completions: std::sync::mpsc::Sender<(u64, Result<LlmResponse, LlmError>)>,
) {
    let mut active = tokio::task::JoinSet::new();
    let mut actor_by_task = HashMap::new();
    let mut accepting = true;

    while accepting || !active.is_empty() {
        tokio::select! {
            biased;
            finished = active.join_next_with_id(), if !active.is_empty() => {
                match finished {
                    Some(Ok((task_id, (actor_id, response)))) => {
                        actor_by_task.remove(&task_id);
                        let _ = completions.send((actor_id, response));
                    }
                    Some(Err(join_error)) => {
                        if let Some(actor_id) = actor_by_task.remove(&join_error.id()) {
                            let _ = completions.send((
                                actor_id,
                                Err(LlmError::from_string("LLM worker task panicked")),
                            ));
                        }
                    }
                    None => {}
                }
            }
            next = requests.recv(), if accepting && active.len() < MAX_CONCURRENT_LLM_REQUESTS => {
                match next {
                    Some(item) => {
                        let actor_id = item.actor_id;
                        let handle = active.spawn(async move {
                            (actor_id, item.client.complete(item.request).await)
                        });
                        actor_by_task.insert(handle.id(), actor_id);
                    }
                    None => accepting = false,
                }
            }
        }
    }
}

impl LlmState {
    /// Set the LLM client provider.
    pub fn set_client(&mut self, client: Box<dyn LlmClient>) {
        self.client = Some(Arc::from(client));
    }

    /// Set a token budget limit. Requests exceeding this are rejected.
    pub fn set_token_budget(&mut self, limit: u64) {
        self.token_budget = Some(Arc::new(TokenBudget::new(limit)));
    }

    /// Remove the token budget limit.
    pub fn clear_token_budget(&mut self) {
        self.token_budget = None;
    }

    /// Check whether the token budget allows the given estimated tokens.
    /// Returns `true` if the request is allowed (budget not exhausted).
    pub fn check_token_budget(&self, _estimated_tokens: u64) -> bool {
        if let Some(budget) = &self.token_budget {
            !budget.is_exhausted()
        } else {
            true
        }
    }

    /// Record token usage against the budget.
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
use crate::primitives::ActorRole;
use crate::runtime::persistence::WorkflowEvent;
use crate::vm::Value;

/// Drain completed background LLM calls and resume any actors waiting for
/// them.
pub(crate) fn poll_llm_completions(rt: &mut Runtime) {
    while let Ok((actor_id, result)) = rt.llm.rx.try_recv() {
        store_llm_completion(rt, actor_id, result);
    }
}

/// Record a completed background LLM call on its actor and resume the
/// actor's suspended behavior, if any. Errors trigger the retry/fallback
/// pipeline when the actor has a configured agent retry or fallback.
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
        Err(error) => {
            handle_llm_error(rt, actor_id, error);
        }
    }
}

/// Process an LLM error: decide whether to retry, fall back, or fail.
pub(crate) fn handle_llm_error(rt: &mut Runtime, actor_id: u64, error: LlmError) {
    // Only agent actors have retry/fallback config.
    let is_agent = rt
        .actors
        .get(&actor_id)
        .map(|a| matches!(a.role(), Ok(ActorRole::Agent)))
        .unwrap_or(false);
    if !is_agent {
        // Non-agent actors: store the error and resume.
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

    // Read retry/fallback config from cached actor fields (parsed once at
    // agent init), plus mutable state for attempt tracking and prompt.
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

    // --- Retry path ---
    if let Some(retry) = &retry_config {
        if attempt < retry.max_attempts {
            let new_attempt = attempt + 1;
            // Update llm_attempt in actor state.
            if let Some(actor) = rt.actors.get_mut(&actor_id) {
                actor.llm_inflight = false; // will be set true again on re-dispatch
                actor.set_state_field("llm_attempt", crate::vm::Value::int(new_attempt as i64));
            }
            let delay_ms = compute_backoff(retry, attempt, actor_id);
            rt.timer_wheel
                .schedule_llm_retry(std::time::Duration::from_millis(delay_ms), actor_id);
            return;
        }
    }

    // --- Fallback path ---
    if fallback_step < fallback_config.len() {
        let error_kind_name = format!("{:?}", error.kind); // "Timeout", "RateLimit", etc.
        let fb = &fallback_config[fallback_step];
        let fb_matches = fb.on.is_empty() || fb.on.iter().any(|k| *k == error_kind_name);
        let new_fallback_step = fallback_step + 1;
        if fb_matches {
            // Swap model and apply context pruning if needed.
            if let Some(actor) = rt.actors.get_mut(&actor_id) {
                actor.llm_inflight = false;
                let model_ptr = actor.allocate_string(&fb.model);
                actor.set_state_field("model", model_ptr);
                actor.set_state_field("llm_attempt", crate::vm::Value::int(0));
                actor.set_state_field(
                    "llm_fallback_step",
                    crate::vm::Value::int(new_fallback_step as i64),
                );
                if let Some(max_tokens) = fb.max_tokens {
                    prune_episodic_memory(rt, actor_id, max_tokens);
                }
            }
            // Re-dispatch the LLM request with the new model.
            redispatch_llm_request(rt, actor_id, &prompt);
            return;
        }
        // Current fallback entry's `on` list didn't match this error;
        // advance to the next entry and retry the decision.
        if let Some(actor) = rt.actors.get_mut(&actor_id) {
            actor.set_state_field("llm_attempt", crate::vm::Value::int(0));
            actor.set_state_field(
                "llm_fallback_step",
                crate::vm::Value::int(new_fallback_step as i64),
            );
        }
        handle_llm_error(rt, actor_id, error);
        return;
    }

    // --- Terminal: all retries and fallbacks exhausted ---
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
    // Clear old pending prompt so re-dispatch doesn't duplicate.
    if let Some(actor) = rt.actors.get_mut(&actor_id) {
        actor.llm_pending_prompt = None;
    }
    redispatch_llm_request(rt, actor_id, &prompt);
}

/// Build and dispatch an LLM request for the actor, marking it in-flight.
pub(crate) fn redispatch_llm_request(rt: &mut Runtime, actor_id: u64, prompt: &str) {
    let is_agent = rt
        .actors
        .get(&actor_id)
        .map(|a| matches!(a.role(), Ok(ActorRole::Agent)))
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
        // Build failed: store nil error and resume.
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
        // Dispatch failed (e.g. worker thread exited): fail gracefully.
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
            }
        }
    }
}

/// Send an LLM request to the persistent worker thread for execution.
/// Returns true if the request was dispatched, false if the worker
/// channel is unavailable (caller should roll back in-flight state).
pub(crate) fn dispatch_llm_request(
    rt: &mut Runtime,
    actor_id: u64,
    request: LlmRequest,
    prompt: &str,
) -> bool {
    let Some(client) = rt.llm.client.clone() else {
        return false;
    };
    let Some(tx) = rt.llm.request_tx.as_ref() else {
        return false;
    };
    // Try admission before touching actor state. A saturated queue must not
    // block the owning scheduler or decrement another actor's inflight count.
    if tx
        .try_send(LlmWorkItem {
            actor_id,
            request,
            client,
        })
        .is_err()
    {
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
/// character-count heuristic (chars / 4). Always preserves the system
/// prompt (which lives in its own state field).
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
                .unwrap_or(crate::vm::Value::nil()),
            Some(module),
        )
        .unwrap_or_default()
    };
    let mut memory: nulang_ai::EpisodicMemory =
        serde_json::from_str(&memory_json).unwrap_or_else(|_| nulang_ai::EpisodicMemory::new(50));

    let max_chars = max_tokens.saturating_mul(4);
    let total_chars: usize = memory.turns.iter().map(|t| t.content.len()).sum();
    while total_chars > max_chars && !memory.turns.is_empty() {
        // Remove oldest non-system turn.
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

/// Resume an actor whose bytecode behavior suspended on
/// `perform LLM.ask` once the background worker has delivered the
/// response. The re-executed `LlmAsk` picks the response up from
/// `actor.llm_completed` via the VM callback.
pub(crate) fn resume_suspended_llm_step(rt: &mut Runtime, actor_id: u64) {
    let suspended = match rt.actors.get_mut(&actor_id) {
        Some(actor) => actor.suspended_execution.take(),
        None => return,
    };
    let Some(suspended) = suspended else { return };

    if rt.vm.is_none() {
        // No VM available; put the suspension back so a later message
        // can re-trigger the step.
        if let Some(actor) = rt.actors.get_mut(&actor_id) {
            actor.suspended_execution = Some(suspended);
        }
        return;
    }

    rt.restore_suspended_workflow_activation(actor_id, suspended.activation);
    let self_ptr: *mut Runtime = rt;
    unsafe {
        let vm = (*self_ptr).vm.as_mut().unwrap();
        // Re-install callbacks bound to THIS actor: other actors may have
        // run on the shared VM while this one was suspended.
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
                // The suspended step ran to completion. For workflow
                // actors record the completion the same way
                // resume_suspended_workflow_step does: clear the
                // suspension marker, advance step_index, append
                // StepCompleted, and checkpoint.
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
                            activation: suspended.activation,
                            step_name: suspended.step_name,
                        },
                    );
                    (*self_ptr).checkpoint_actor(actor_id);
                }
            }
            Err(crate::types::NuError::Suspended(_)) => {
                // Suspended again (e.g. a chained `perform LLM.ask` or a
                // signal wait): re-capture the VM state so the next
                // completion or signal can resume it.
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
                                activation: suspended.activation,
                                step_name: suspended.step_name,
                            });
                    }
                    // A chained receive-after suspend arms its timeout
                    // here; a no-op for the other sentinels.
                    (*self_ptr).maybe_schedule_receive_wait(actor_id, receive_timeout);
                }
            }
            // Other errors: the send-path result is discarded anyway,
            // matching step_actor semantics.
            Err(_) => {}
        }
        // End the VM-execution window only after any suspend-state
        // re-capture above: draining deferred wakes runs other actors
        // on the shared VM, which would clobber the frames an
        // un-captured suspend still needs. Runs on every path, so
        // wakes of other actors are not lost when THIS one suspends.
        (*self_ptr).vm_exec_end();
    }
    // The suspension resolved (completed or failed): if messages queued
    // up while the behavior was suspended, schedule the actor to drain
    // them — step_actor leaves mail untouched while a suspension is live.
    rt.requeue_if_mail_pending(actor_id);
}

#[cfg(test)]
mod llm_concurrency_tests {
    use super::*;
    use std::sync::{mpsc, Arc};
    use std::time::Duration;
    use tokio::sync::Semaphore;

    struct GatedAsyncClient {
        started: mpsc::Sender<()>,
        gate: Arc<Semaphore>,
    }

    #[async_trait::async_trait]
    impl LlmClient for GatedAsyncClient {
        async fn complete(&self, _request: LlmRequest) -> Result<LlmResponse, LlmError> {
            self.started.send(()).expect("test receiver dropped");
            let _permit = self
                .gate
                .clone()
                .acquire_owned()
                .await
                .expect("test gate closed");
            Ok(LlmResponse {
                content: Some("ok".to_string()),
                tool_calls: vec![],
                model: "test".to_string(),
                finish_reason: "stop".to_string(),
                usage: Default::default(),
            })
        }
    }

    struct PanickingAsyncClient;

    #[async_trait::async_trait]
    impl LlmClient for PanickingAsyncClient {
        async fn complete(&self, _request: LlmRequest) -> Result<LlmResponse, LlmError> {
            panic!("deliberate test-only provider panic")
        }
    }

    #[test]
    fn worker_queue_is_bounded_without_unsafe_message_transfer() {
        fn assert_send<T: Send>() {}
        assert_send::<LlmWorkItem>();
        let state = LlmState::new();
        assert_eq!(state.request_tx.as_ref().unwrap().max_capacity(), 64);
    }

    #[test]
    fn rejected_llm_work_never_changes_inflight_accounting() {
        let mut runtime = Runtime::new();
        runtime.llm.client = Some(Arc::new(nulang_ai::MockLlmClient::text("ok")));
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        runtime.llm.request_tx = Some(tx);
        assert!(dispatch_llm_request(
            &mut runtime,
            100,
            LlmRequest::default(),
            "accepted",
        ));
        assert_eq!(runtime.llm.inflight_count, 1);
        assert!(!dispatch_llm_request(
            &mut runtime,
            101,
            LlmRequest::default(),
            "full",
        ));
        assert_eq!(runtime.llm.inflight_count, 1);

        drop(rx);
        assert!(!dispatch_llm_request(
            &mut runtime,
            102,
            LlmRequest::default(),
            "disconnected",
        ));
        assert_eq!(runtime.llm.inflight_count, 1);
    }

    #[test]
    fn a_panicking_request_returns_an_error_without_stopping_the_worker() {
        let mut state = LlmState::new();
        let normal: Arc<dyn LlmClient> = Arc::new(nulang_ai::MockLlmClient::text("still alive"));
        let panicking: Arc<dyn LlmClient> = Arc::new(PanickingAsyncClient);
        for (actor_id, client) in [(21, panicking), (22, normal)] {
            state
                .request_tx
                .as_ref()
                .unwrap()
                .try_send(LlmWorkItem {
                    actor_id,
                    request: LlmRequest::default(),
                    client,
                })
                .expect("request admitted");
        }
        let mut responses = Vec::new();
        for _ in 0..2 {
            responses.push(
                state
                    .rx
                    .recv_timeout(Duration::from_secs(5))
                    .expect("every started request must complete, even after panic"),
            );
        }
        responses.sort_unstable_by_key(|(actor_id, _)| *actor_id);
        assert_eq!(responses[0].0, 21);
        assert!(responses[0].1.is_err());
        assert_eq!(responses[1].0, 22);
        assert_eq!(
            responses[1].1.as_ref().unwrap().content.as_deref(),
            Some("still alive")
        );
        state.request_tx.take();
    }

    #[test]
    fn independent_async_llm_requests_make_progress_concurrently() {
        let mut state = LlmState::new();
        let (started_tx, started_rx) = mpsc::channel();
        let gate = Arc::new(Semaphore::new(0));
        let client: Arc<dyn LlmClient> = Arc::new(GatedAsyncClient {
            started: started_tx,
            gate: Arc::clone(&gate),
        });
        for actor_id in [11, 12] {
            state
                .request_tx
                .as_ref()
                .unwrap()
                .try_send(LlmWorkItem {
                    actor_id,
                    request: LlmRequest::default(),
                    client: Arc::clone(&client),
                })
                .expect("request admitted");
        }
        // A serial worker can enter only the first provider call while the
        // gate is held; a bounded async worker enters both before release.
        let concurrent = (0..2).all(|_| started_rx.recv_timeout(Duration::from_secs(5)).is_ok());
        gate.add_permits(2);
        let mut ids = Vec::new();
        for _ in 0..2 {
            let (actor_id, result) = state
                .rx
                .recv_timeout(Duration::from_secs(5))
                .expect("completion after gate release");
            assert_eq!(
                result.expect("provider response").content.as_deref(),
                Some("ok")
            );
            ids.push(actor_id);
        }
        ids.sort_unstable();
        assert_eq!(ids, [11, 12]);
        assert!(concurrent, "two independent async requests must overlap");
        state.request_tx.take();
    }
}
