//! Agent subsystem: LLM client management, token budgets, pipelines,
//! supervisor teams, debates, and the agent LLM completion pipeline.
//!
//! All functions in this module take `&Runtime` or `&mut Runtime` and
//! access its public fields directly. This extraction follows the pattern
//! established by `workflow.rs`, `exit.rs`, `distribution.rs`, and
//! `spawn.rs` — shrink the Runtime god-object (`mod.rs`) by moving
//! self-contained subsystems into their own modules.
//!
//! These methods are classified as Experimental (RFC 0004). They remain
//! functional during the deprecation cycle.

use std::sync::Arc;

use crate::runtime::Runtime;
use nulang_ai::{
    EpisodicMemory, LlmClient, LlmMessage, LlmRequest, LlmResponse, ModelPricing, TokenBudget,
};

/// Convert core `ToolSchema` (bytecode/HIR, unconditional) into the
/// `nulang-ai` wire-format `ToolSchema` used by `LlmRequest.tools`. The two
/// types are structurally identical but independently defined: `nulang-ai`
/// has zero dependency on core `nulang`, so core cannot hand it its own
/// type directly.
fn to_provider_tool_schema(t: &crate::tool_schema::ToolSchema) -> nulang_ai::ToolSchema {
    nulang_ai::ToolSchema {
        name: t.name.clone(),
        description: t.description.clone(),
        parameters: t.parameters.clone(),
    }
}

// ---------------------------------------------------------------------------
// LLM client + token budget
// ---------------------------------------------------------------------------

/// Install an LLM client for `perform LLM.ask(...)` calls.
pub(crate) fn set_llm_client(rt: &mut Runtime, client: Box<dyn LlmClient>) {
    rt.llm.client = Some(Arc::from(client));
}

/// Set a token budget that caps total LLM token consumption.
///
/// After the budget is exhausted `complete_llm_request` returns
/// `LlmError::BudgetExceeded`.  Charges are applied after each
/// successful response based on the actual token count returned
/// by the provider.
pub(crate) fn set_token_budget(rt: &mut Runtime, limit: u64) {
    rt.llm.token_budget = Some(std::sync::Arc::new(TokenBudget::new(limit)));
}

/// Remove any configured token budget.
pub(crate) fn clear_token_budget(rt: &mut Runtime) {
    rt.llm.token_budget = None;
}

// ---------------------------------------------------------------------------
// Pipeline
// ---------------------------------------------------------------------------

/// Create a new empty pipeline and return its ID.
pub(crate) fn pipeline_new(rt: &mut Runtime) -> u64 {
    rt.ai.create_pipeline()
}

/// Add a stage to an existing pipeline. Returns the same pipeline ID on
/// success so fluent construction can continue.
pub(crate) fn pipeline_stage(
    rt: &mut Runtime,
    id: u64,
    name: &str,
    agent_id: u64,
    template: &str,
) -> Result<u64, String> {
    rt.ai.add_pipeline_stage(id, name, agent_id, template)
}

/// Run a pipeline, returning the output of the final stage.
pub(crate) fn pipeline_run(rt: &mut Runtime, id: u64, input: &str) -> Result<String, String> {
    let pipeline = rt
        .ai
        .pipelines
        .get(&id)
        .cloned()
        .ok_or_else(|| format!("Pipeline {} not found", id))?;
    pipeline.run(rt, input)
}

// ---------------------------------------------------------------------------
// Supervisor teams
// ---------------------------------------------------------------------------

pub(crate) fn supervisor_new(rt: &mut Runtime) -> u64 {
    rt.supervisor_teams.create()
}

pub(crate) fn supervisor_worker(
    rt: &mut Runtime,
    id: u64,
    name: &str,
    agent_id: u64,
    description: &str,
) -> Result<u64, String> {
    rt.supervisor_teams
        .add_worker(id, name, agent_id, description)
}

pub(crate) fn supervisor_run(rt: &mut Runtime, id: u64, task: &str) -> Result<String, String> {
    let team = rt
        .supervisor_teams
        .teams
        .get(&id)
        .cloned()
        .ok_or_else(|| format!("Supervisor team {} not found", id))?;
    team.run(rt, task)
}

// ---------------------------------------------------------------------------
// Debates
// ---------------------------------------------------------------------------

/// Create a new debate and return its ID.
pub(crate) fn debate_new(rt: &mut Runtime, topic: &str, rounds: i64, threshold: f64) -> u64 {
    rt.ai.create_debate(topic, rounds, threshold)
}

/// Add a participant to an existing debate. Returns the same debate ID on
/// success so fluent construction can continue.
pub(crate) fn debate_participant(
    rt: &mut Runtime,
    id: u64,
    name: &str,
    stance: &str,
    agent_id: u64,
) -> Result<u64, String> {
    rt.ai.add_debate_participant(id, name, stance, agent_id)
}

/// Run a debate and return the moderator's synthesis.
pub(crate) fn debate_run(rt: &mut Runtime, id: u64) -> Result<String, String> {
    let debate = rt
        .ai
        .debates
        .get(&id)
        .cloned()
        .ok_or_else(|| format!("Debate {} not found", id))?;
    debate.run(rt)
}

// ---------------------------------------------------------------------------
// VM value ↔ string helpers
// ---------------------------------------------------------------------------

/// Convert a VM value to a Rust string using the actor's bytecode module
/// constant pool for string-id values and reading pointer payloads as
/// null-terminated UTF-8.
pub(crate) fn vm_value_to_string(
    value: &crate::vm::Value,
    module: Option<&crate::bytecode::CodeModule>,
) -> Option<String> {
    if let Some(id) = value.as_string_id() {
        module
            .and_then(|m| m.constants.get(id as usize))
            .and_then(|c| match c {
                crate::bytecode::Constant::String(s) => Some(s.clone()),
                _ => None,
            })
    } else if let Some(ptr) = value.as_ptr() {
        if ptr.is_null() {
            Some(String::new())
        } else {
            Some(unsafe {
                std::ffi::CStr::from_ptr(ptr as *const std::ffi::c_char)
                    .to_string_lossy()
                    .into_owned()
            })
        }
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Agent LLM completion pipeline
// ---------------------------------------------------------------------------

/// Execute an LLM request for an agent actor, reading the agent's model,
/// system prompt, and episodic memory from durable state. The memory is
/// updated with the user prompt and assistant response before being saved
/// back to state.
pub(crate) fn complete_agent_llm(rt: &mut Runtime, actor_id: u64, prompt: &str) -> Option<String> {
    let prev_current_actor = rt.current_actor;
    rt.current_actor = Some(actor_id);

    let result = complete_agent_llm_inner(rt, actor_id, prompt);

    rt.current_actor = prev_current_actor;
    result
}

fn complete_agent_llm_inner(rt: &mut Runtime, actor_id: u64, prompt: &str) -> Option<String> {
    let request = build_agent_llm_request(rt, actor_id, prompt)?;
    let module = rt.actors.get(&actor_id)?.bytecode_module.clone()?;
    let response = rt
        .complete_llm_with_tools(request, Vec::new(), &module)
        .ok()?;
    finish_agent_llm(rt, actor_id, prompt, &response)
}

/// Build the LLM request for an agent actor from its durable state
/// (model, system prompt, episodic memory, pricing) without issuing any
/// network call. Pure read/build: safe to run before handing the request
/// to a background worker thread.
pub(crate) fn build_agent_llm_request(
    rt: &Runtime,
    actor_id: u64,
    prompt: &str,
) -> Option<LlmRequest> {
    let (model, system_prompt, memory_json, pricing, module) = {
        let actor = rt.actors.get(&actor_id)?;
        let module = actor.bytecode_module.clone()?;
        let model = vm_value_to_string(&actor.get_state_field("model")?, Some(&module))?;
        let system_prompt =
            vm_value_to_string(&actor.get_state_field("system_prompt")?, Some(&module))?;
        let memory_json =
            vm_value_to_string(&actor.get_state_field("episodic_memory")?, Some(&module))?;
        let pricing = ModelPricing {
            input_cost_per_1k: actor.get_state_field("pricing_input")?.as_float()?,
            output_cost_per_1k: actor.get_state_field("pricing_output")?.as_float()?,
        };
        (model, system_prompt, memory_json, pricing, module)
    };

    let memory: EpisodicMemory =
        serde_json::from_str(&memory_json).unwrap_or_else(|_| EpisodicMemory::new(50));

    let mut messages = Vec::new();
    if !system_prompt.is_empty() {
        messages.push(LlmMessage {
            role: "system".to_string(),
            content: system_prompt,
        });
    }
    messages.extend(memory.to_messages());
    messages.push(LlmMessage {
        role: "user".to_string(),
        content: prompt.to_string(),
    });

    Some(LlmRequest {
        model,
        messages,
        tools: module.tools.iter().map(to_provider_tool_schema).collect(),
        memory: Vec::new(),
        pricing: Some(pricing),
        response_format: None,
    })
}

/// Finish an agent LLM call on the scheduler thread: accumulate token
/// usage and cost, append the exchange to episodic memory, and write the
/// durable state back. Returns the response content. Episodic memory is
/// re-read fresh here (never reuse the build-time snapshot).
pub(crate) fn finish_agent_llm(
    rt: &mut Runtime,
    actor_id: u64,
    prompt: &str,
    response: &LlmResponse,
) -> Option<String> {
    let (pricing, usage_prompt, usage_completion, usage_cost, memory_json) = {
        let actor = rt.actors.get(&actor_id)?;
        let module = actor.bytecode_module.clone()?;
        let pricing = ModelPricing {
            input_cost_per_1k: actor.get_state_field("pricing_input")?.as_float()?,
            output_cost_per_1k: actor.get_state_field("pricing_output")?.as_float()?,
        };
        let usage_prompt = actor.get_state_field("usage_prompt")?.as_int()? as u32;
        let usage_completion = actor.get_state_field("usage_completion")?.as_int()? as u32;
        let usage_cost = actor.get_state_field("usage_cost")?.as_float()?;
        let memory_json =
            vm_value_to_string(&actor.get_state_field("episodic_memory")?, Some(&module))?;
        (
            pricing,
            usage_prompt,
            usage_completion,
            usage_cost,
            memory_json,
        )
    };
    let content = response.content.clone().unwrap_or_default();

    // Accumulate token usage and cost into durable state.
    let new_cost = nulang_ai::estimated_cost(&response.usage, &pricing);
    let updated_prompt = usage_prompt.saturating_add(response.usage.prompt);
    let updated_completion = usage_completion.saturating_add(response.usage.completion);
    let updated_cost = usage_cost + new_cost;

    let mut memory: EpisodicMemory =
        serde_json::from_str(&memory_json).unwrap_or_else(|_| EpisodicMemory::new(50));
    memory.add_turn("user", prompt);
    memory.add_turn("assistant", &content);
    let updated_memory = serde_json::to_string(&memory).ok()?;

    let actor = rt.actors.get_mut(&actor_id)?;
    let ptr = actor.allocate_string(&updated_memory);
    actor.set_state_field("episodic_memory", ptr);
    actor.set_state_field("usage_prompt", crate::vm::Value::int(updated_prompt as i64));
    actor.set_state_field(
        "usage_completion",
        crate::vm::Value::int(updated_completion as i64),
    );
    actor.set_state_field("usage_cost", crate::vm::Value::float(updated_cost));
    Some(content)
}

/// Build a bare LLM request for a non-agent actor bytecode behavior,
/// with `tools` filled from the actor's bytecode module. Pure
/// read/build: safe to run before handing the request to a background
/// worker thread.
pub(crate) fn build_actor_llm_request(
    rt: &Runtime,
    actor_id: u64,
    model: &str,
    prompt: &str,
) -> Option<LlmRequest> {
    let module = rt.actors.get(&actor_id)?.bytecode_module.clone()?;
    Some(LlmRequest {
        model: model.to_string(),
        messages: vec![LlmMessage {
            role: "user".to_string(),
            content: prompt.to_string(),
        }],
        tools: module.tools.iter().map(to_provider_tool_schema).collect(),
        memory: Vec::new(),
        pricing: None,
        response_format: None,
    })
}

/// Read an actor's state field as a plain string, resolving string-id
/// values through the runtime VM's constant pools (heap pointer values
/// are read directly). Useful for tests and tooling that inspect actor
/// state produced by bytecode behaviors.
pub(crate) fn actor_state_string(rt: &Runtime, actor_id: u64, field: &str) -> Option<String> {
    let actor = rt.actors.get(&actor_id)?;
    let value = actor.get_state_field(field)?;
    if value.as_string_id().is_some() {
        let vm = rt.vm.as_ref()?;
        let module_idx = actor.bytecode_module_idx?;
        return Some(vm.value_to_string(module_idx, value));
    }
    vm_value_to_string(&value, actor.bytecode_module.as_deref())
}

// ---------------------------------------------------------------------------
// Tool-calling: LLM chat completion + agent tool invocation
// ---------------------------------------------------------------------------

/// Execute a chat-completion request using the configured LLM client.
///
/// The provided `memory` messages are stored on the request before it is
/// sent to the provider.
pub(crate) fn complete_llm_request(
    rt: &Runtime,
    mut request: LlmRequest,
    memory: Vec<LlmMessage>,
) -> Result<LlmResponse, nulang_ai::LlmError> {
    // Check token budget before calling the provider.
    if let Some(budget) = &rt.llm.token_budget {
        if budget.is_exhausted() {
            return Err(nulang_ai::LlmError::new(
                nulang_ai::LlmErrorKind::BudgetExceeded,
                format!("Token budget exhausted (limit: {})", budget.limit()),
            ));
        }
    }
    request.memory = memory;
    let client = rt
        .llm
        .client
        .as_ref()
        .ok_or_else(|| nulang_ai::LlmError::from_string("No LLM client configured"))?;
    let response = nulang_ai::complete_sync(client.as_ref(), request)?;
    // Charge the budget for actual tokens consumed.
    if let Some(budget) = &rt.llm.token_budget {
        budget.charge(response.usage.total as u64);
    }
    Ok(response)
}

/// Execute an LLM request, optionally running tool calls from the response.
///
/// The request's `tools` list is populated from `module.tools`. If the
/// response contains tool calls, the named functions are looked up in the
/// module exports, invoked with the provided JSON arguments, and the results
/// are sent back to the model for a final response. The supplied `memory`
/// messages are preserved across tool-call rounds.
pub(crate) fn complete_llm_with_tools(
    rt: &mut Runtime,
    mut request: LlmRequest,
    memory: Vec<LlmMessage>,
    module: &crate::bytecode::CodeModule,
) -> Result<LlmResponse, nulang_ai::LlmError> {
    request.tools = module.tools.iter().map(to_provider_tool_schema).collect();
    request.memory = memory.clone();
    let response = complete_llm_request(rt, request.clone(), memory.clone())?;
    finish_tool_calls(rt, module, response)
}

/// Post-process an LLM response on the scheduler thread: invoke any tool
/// calls named in the response against `module` and synthesize the
/// response content from their results. Must run on the scheduler thread
/// because tool invocation executes module functions against runtime
/// state.
pub(crate) fn finish_tool_calls(
    rt: &mut Runtime,
    module: &crate::bytecode::CodeModule,
    mut response: LlmResponse,
) -> Result<LlmResponse, nulang_ai::LlmError> {
    if !response.tool_calls.is_empty() {
        let mut results = Vec::new();
        for call in &response.tool_calls {
            let result = invoke_agent_tool_function(rt, module, &call.name, &call.arguments)?;
            results.push((call.name.clone(), result));
        }

        // For agent workflows, return the tool results directly so the
        // caller can decide whether to continue the conversation. Preserve
        // the original tool_calls and usage while surfacing a synthesized
        // content string for memory/logging.
        let result_content = results
            .iter()
            .map(|(name, result)| format!("{}: {}", name, result))
            .collect::<Vec<_>>()
            .join("\n");
        response.content = Some(result_content);
    }

    Ok(response)
}

/// Invoke a tool for an agent, routing memory behaviors to the agent's
/// durable state and falling back to the module's exported function for
/// other tools.
pub(crate) fn invoke_agent_tool_function(
    rt: &mut Runtime,
    module: &crate::bytecode::CodeModule,
    name: &str,
    arguments: &serde_json::Map<String, serde_json::Value>,
) -> Result<String, String> {
    if let Some(actor_id) = rt.current_actor {
        if rt.actor_is_agent(actor_id) && rt.is_semantic_memory_behavior(name) {
            return invoke_semantic_memory_tool(rt, actor_id, name, arguments);
        }
        if rt.actor_is_agent(actor_id) && rt.is_procedural_memory_behavior(name) {
            return invoke_procedural_memory_tool(rt, actor_id, name, arguments);
        }
    }
    invoke_tool_function(rt, module, name, arguments)
}

/// Execute a semantic-memory tool call against the current agent.
pub(crate) fn invoke_semantic_memory_tool(
    rt: &mut Runtime,
    actor_id: u64,
    name: &str,
    arguments: &serde_json::Map<String, serde_json::Value>,
) -> Result<String, String> {
    if name == "store_fact" {
        let content = arguments
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let mut metadata = std::collections::HashMap::new();
        if let Some(topic) = arguments.get("topic").and_then(|v| v.as_str()) {
            metadata.insert("topic".to_string(), topic.to_string());
        }
        let id = rt.semantic_memory_store_with_metadata(actor_id, &content, metadata);
        Ok(format!(
            "stored: {}",
            vm_value_to_string_or_default(rt, actor_id, &id)
        ))
    } else if name == "recall" {
        let query = arguments
            .get("query")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let top_k = arguments.get("top_k").and_then(|v| v.as_u64()).unwrap_or(1) as usize;
        let value = rt.semantic_memory_recall(actor_id, &query, top_k);
        Ok(vm_value_to_string_or_default(rt, actor_id, &value))
    } else {
        Err(format!("Unknown semantic-memory tool '{}'", name))
    }
}

/// Execute a procedural-memory tool call against the current agent.
pub(crate) fn invoke_procedural_memory_tool(
    rt: &mut Runtime,
    actor_id: u64,
    name: &str,
    arguments: &serde_json::Map<String, serde_json::Value>,
) -> Result<String, String> {
    match name {
        "store_pattern" => {
            let key = arguments
                .get("key")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let input_pattern = arguments
                .get("input_pattern")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let output_template = arguments
                .get("output_template")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let value = rt.procedural_memory_store_pattern(
                actor_id,
                &key,
                &input_pattern,
                &output_template,
            );
            Ok(vm_value_to_string_or_default(rt, actor_id, &value))
        }
        "get_pattern" => {
            let key = arguments
                .get("key")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let value = rt.procedural_memory_get_pattern(actor_id, &key);
            Ok(vm_value_to_string_or_default(rt, actor_id, &value))
        }
        "add_example" => {
            let task = arguments
                .get("task")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let input = arguments
                .get("input")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let output = arguments
                .get("output")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            rt.procedural_memory_add_example(actor_id, &task, &input, &output);
            Ok("ok".to_string())
        }
        "get_examples" => {
            let task = arguments
                .get("task")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let query = arguments
                .get("query")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let top_k = arguments.get("top_k").and_then(|v| v.as_u64()).unwrap_or(1) as usize;
            let value = rt.procedural_memory_get_examples(actor_id, &task, &query, top_k);
            Ok(vm_value_to_string_or_default(rt, actor_id, &value))
        }
        _ => Err(format!("Unknown procedural-memory tool '{}'", name)),
    }
}

/// Convert a VM value into a Rust string, returning a default for missing actors.
pub(crate) fn vm_value_to_string_or_default(
    rt: &Runtime,
    actor_id: u64,
    value: &crate::vm::Value,
) -> String {
    rt.actors
        .get(&actor_id)
        .and_then(|actor| rt.vm_value_to_string_in_actor(value, actor))
        .unwrap_or_default()
}

/// Look up a tool by name and invoke the corresponding exported function.
pub(crate) fn invoke_tool_function(
    _rt: &Runtime,
    module: &crate::bytecode::CodeModule,
    name: &str,
    arguments: &serde_json::Map<String, serde_json::Value>,
) -> Result<String, String> {
    let tool = module
        .tools
        .iter()
        .find(|t| t.name == name)
        .ok_or_else(|| format!("Tool '{}' not found", name))?;

    let export_idx = module
        .exports
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, idx)| *idx)
        .ok_or_else(|| format!("Tool function '{}' is not exported", name))?;

    let func_idx = match module.constants.get(export_idx) {
        Some(crate::bytecode::Constant::FunctionRef(idx)) => *idx,
        _ => return Err(format!("Export '{}' is not a function reference", name)),
    };

    let offset = *module
        .function_table
        .get(func_idx)
        .ok_or_else(|| format!("Function table missing entry for '{}'", name))?;

    let properties = tool
        .parameters
        .get("properties")
        .and_then(|v| v.as_object())
        .ok_or_else(|| format!("Tool '{}' has no parameter schema", name))?;

    let mut vm = crate::vm::VM::new();
    vm.load_module(module.clone());
    let module_idx = 0;
    let mut frame = crate::vm::Frame::new(None, module_idx);
    frame.pc = offset;

    for (i, (param_name, _)) in properties.iter().enumerate().take(256) {
        let json_val = arguments
            .get(param_name)
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        frame.regs[i] = crate::runtime::json_to_vm_value(&mut vm, json_val)?;
    }

    vm.set_current_frame(frame);
    let result = vm
        .run_from(module_idx, offset)
        .map_err(|e| format!("Tool '{}' execution failed: {}", name, e))?;
    Ok(vm.value_to_string(module_idx, result))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::Runtime;

    #[test]
    fn test_set_clear_token_budget() {
        let mut rt = Runtime::new();
        assert!(rt.llm.token_budget.is_none());
        set_token_budget(&mut rt, 1000);
        assert!(rt.llm.token_budget.is_some());
        clear_token_budget(&mut rt);
        assert!(rt.llm.token_budget.is_none());
    }

    #[test]
    fn test_pipeline_new_and_stage() {
        let mut rt = Runtime::new();
        let id = pipeline_new(&mut rt);
        // Add a stage — needs a valid agent_id; use 0 as placeholder
        let result = pipeline_stage(&mut rt, id, "test_stage", 0, "Hello");
        assert!(result.is_ok());
    }

    #[test]
    fn test_debate_new_and_participant() {
        let mut rt = Runtime::new();
        let id = debate_new(&mut rt, "test topic", 3, 0.6);
        let result = debate_participant(&mut rt, id, "Alice", "pro", 0);
        assert!(result.is_ok());
    }

    #[test]
    fn test_supervisor_new_and_worker() {
        let mut rt = Runtime::new();
        let id = supervisor_new(&mut rt);
        let result = supervisor_worker(&mut rt, id, "worker1", 0, "does stuff");
        assert!(result.is_ok());
    }
}
