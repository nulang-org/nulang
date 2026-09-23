//! LLM subsystem for the actor runtime.
//!
//! Manages the persistent LLM worker thread, request dispatch, completion
//! polling, and non-blocking suspension for `perform LLM.ask` in bytecode
//! behaviors.

use std::sync::Arc;

use nulang_ai::{LlmClient, LlmError, LlmRequest, LlmResponse, TokenBudget};

/// Work item sent to the persistent LLM worker thread.
pub(crate) struct LlmWorkItem {
    pub(crate) actor_id: u64,
    pub(crate) request: LlmRequest,
    pub(crate) client: Arc<dyn LlmClient>,
}

// Safety: LlmWorkItem is Send because all fields are Send.
unsafe impl Send for LlmWorkItem {}

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

    /// Channel to dispatch work to the persistent LLM worker thread.
    /// `None` after the runtime is dropped (sender half is owned by the
    /// worker thread, which outlives the runtime).
    pub(crate) request_tx: Option<crossbeam::channel::Sender<LlmWorkItem>>,
}

impl LlmState {
    /// Create the LLM subsystem, spawning the persistent worker thread.
    ///
    /// The worker thread owns its own single-threaded tokio runtime and
    /// processes requests sequentially. Results are sent back through the
    /// `rx` channel.
    pub fn new() -> Self {
        let (llm_tx, llm_rx) = std::sync::mpsc::channel();
        let llm_tx_worker = llm_tx.clone();
        let (llm_request_tx, llm_request_rx) = crossbeam::channel::unbounded::<LlmWorkItem>();

        // Spawn a persistent LLM worker thread.
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
                while let Ok(item) = llm_request_rx.recv() {
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        tokio_rt.block_on(item.client.complete(item.request))
                    }))
                    .unwrap_or_else(|_| Err(LlmError::from_string("LLM worker thread panicked")));
                    let _ = llm_tx_worker.send((item.actor_id, result));
                }
            });

        LlmState {
            token_budget: None,
            client: None,
            rx: llm_rx,
            inflight_count: 0,
            request_tx: Some(llm_request_tx),
        }
    }

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
    if let Some(actor) = rt.actors.get_mut(&actor_id) {
        actor.llm_inflight = true;
        actor.llm_pending_prompt = Some(prompt.to_string());
    }
    rt.llm.inflight_count += 1;
    tx.send(LlmWorkItem {
        actor_id,
        request,
        client,
    })
    .is_ok()
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
