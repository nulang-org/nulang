//! Runtime host for the versioned workflow control protocol.
//!
//! The host maps opaque protocol workflow identities onto the existing Nulang
//! actor runtime. It does not implement workflow execution: step ordering,
//! suspension, durable events, timers, signals, and saga compensation remain
//! owned by `Runtime`.

use super::Runtime;
use crate::bytecode::CodeModule;
use crate::types::ExitReason;
use crate::vm::Value;
use nulang_workflow_protocol::{
    WorkflowControlCommand, WorkflowControlError, WorkflowControlErrorCode, WorkflowControlReply,
    WorkflowControlRequest, WorkflowControlResponse, WorkflowControlResult, WorkflowDefinitionId,
    WorkflowFailure, WorkflowInstanceId, WorkflowLifecycleStatus, WorkflowSnapshot, WorkflowWait,
};
use serde_json::Value as JsonValue;
use std::collections::HashMap;

#[derive(Clone)]
struct RegisteredWorkflow {
    module: CodeModule,
    first_behavior_idx: usize,
    step_names: Vec<String>,
}

#[derive(Clone)]
struct InstanceBinding {
    actor_id: u64,
    definition_id: WorkflowDefinitionId,
    step_names: Vec<String>,
    cancelled: bool,
}

/// Transport-neutral host for `nulang-workflow-control/v0alpha1`.
///
/// Definition registration is intentionally host-local and outside the wire
/// protocol: deployment/admission decides which compiled modules may run, while
/// the protocol only controls instances of already-admitted definitions.
#[derive(Default)]
pub struct WorkflowControlHost {
    definitions: HashMap<WorkflowDefinitionId, RegisteredWorkflow>,
    instances: HashMap<WorkflowInstanceId, InstanceBinding>,
    idempotency: HashMap<String, WorkflowInstanceId>,
}

impl WorkflowControlHost {
    pub fn new() -> Self {
        Self::default()
    }

    /// Admit one compiled workflow module under an opaque host definition ID.
    ///
    /// A module containing multiple workflows must name the requested workflow
    /// exactly; a single-workflow module may be registered under an arbitrary
    /// external definition ID.
    pub fn register_definition(
        &mut self,
        definition_id: WorkflowDefinitionId,
        module: CodeModule,
    ) -> Result<(), WorkflowControlError> {
        let workflow_metas: Vec<_> = module
            .actor_metadata
            .iter()
            .filter(|meta| meta.is_workflow)
            .collect();

        let meta = workflow_metas
            .iter()
            .copied()
            .find(|meta| meta.name == definition_id.as_str())
            .or_else(|| {
                if workflow_metas.len() == 1 {
                    workflow_metas.first().copied()
                } else {
                    None
                }
            })
            .ok_or_else(|| {
                protocol_error(
                    WorkflowControlErrorCode::InvalidRequest,
                    "definition module does not contain an unambiguous workflow",
                    false,
                )
            })?;

        let first_behavior_idx = meta.behavior_indices.first().copied().ok_or_else(|| {
            protocol_error(
                WorkflowControlErrorCode::InvalidRequest,
                "workflow definition has no steps",
                false,
            )
        })?;

        if meta.behavior_indices.len() > u16::MAX as usize {
            return Err(protocol_error(
                WorkflowControlErrorCode::InvalidRequest,
                "workflow definition exceeds the runtime behavior-id limit",
                false,
            ));
        }

        let step_names = meta
            .behavior_indices
            .iter()
            .map(|&idx| {
                module
                    .behaviors
                    .get(idx)
                    .map(|entry| short_behavior_name(&entry.name))
                    .ok_or_else(|| {
                        protocol_error(
                            WorkflowControlErrorCode::InvalidRequest,
                            "workflow metadata references a missing behavior",
                            false,
                        )
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;

        self.definitions.insert(
            definition_id,
            RegisteredWorkflow {
                module,
                first_behavior_idx,
                step_names,
            },
        );
        Ok(())
    }

    pub fn actor_id_for(&self, instance_id: &WorkflowInstanceId) -> Option<u64> {
        self.instances.get(instance_id).map(|binding| binding.actor_id)
    }

    pub fn handle(
        &mut self,
        runtime: &mut Runtime,
        request: WorkflowControlRequest,
    ) -> WorkflowControlResponse {
        let request_id = request.request_id.clone();
        if let Err(error) = request.validate() {
            return WorkflowControlResponse::error(
                request_id,
                protocol_error(
                    WorkflowControlErrorCode::Unsupported,
                    error.to_string(),
                    false,
                ),
            );
        }

        match self.handle_command(runtime, request.command) {
            Ok(reply) => WorkflowControlResponse::ok(request_id, reply),
            Err(error) => WorkflowControlResponse::error(request_id, error),
        }
    }

    fn handle_command(
        &mut self,
        runtime: &mut Runtime,
        command: WorkflowControlCommand,
    ) -> Result<WorkflowControlReply, WorkflowControlError> {
        match command {
            WorkflowControlCommand::Start {
                definition_id,
                instance_id,
                input,
                idempotency_key,
            } => {
                if !input.is_empty() {
                    return Err(protocol_error(
                        WorkflowControlErrorCode::InvalidRequest,
                        "workflow runtime input mapping is not available in v0alpha1",
                        false,
                    ));
                }

                if let Some(key) = idempotency_key.as_ref() {
                    if let Some(existing_id) = self.idempotency.get(key).cloned() {
                        let workflow = self.snapshot(runtime, &existing_id)?;
                        return Ok(WorkflowControlReply::Started { workflow });
                    }
                }

                if let Some(existing_id) = instance_id.as_ref() {
                    if let Some(existing) = self.instances.get(existing_id) {
                        if existing.definition_id != definition_id {
                            return Err(protocol_error(
                                WorkflowControlErrorCode::Conflict,
                                "workflow instance id is already bound to another definition",
                                false,
                            ));
                        }
                        let workflow = self.snapshot(runtime, existing_id)?;
                        return Ok(WorkflowControlReply::Started { workflow });
                    }
                }

                let definition = self.definitions.get(&definition_id).cloned().ok_or_else(|| {
                    protocol_error(
                        WorkflowControlErrorCode::NotFound,
                        format!("workflow definition not found: {definition_id}"),
                        false,
                    )
                })?;

                let actor_value = runtime.spawn_from_module(
                    &definition.module,
                    definition.first_behavior_idx,
                    Vec::new(),
                );
                let actor_id = actor_value.as_actor_id().ok_or_else(|| {
                    protocol_error(
                        WorkflowControlErrorCode::Unavailable,
                        "workflow actor could not be durably created",
                        true,
                    )
                })?;

                let instance_id = instance_id
                    .unwrap_or_else(|| WorkflowInstanceId::new(format!("actor-{actor_id}")));
                self.instances.insert(
                    instance_id.clone(),
                    InstanceBinding {
                        actor_id,
                        definition_id: definition_id.clone(),
                        step_names: definition.step_names.clone(),
                        cancelled: false,
                    },
                );
                if let Some(key) = idempotency_key {
                    self.idempotency.insert(key, instance_id.clone());
                }

                // Workflow actor behavior ids are actor-local and compressed to
                // 0..step_count-1 by Runtime::layout_workflow_behavior_table.
                // Queueing the declared steps preserves the runtime's existing
                // mailbox-first workflow execution model; a suspended step owns
                // the actor and prevents later queued steps from running until
                // the wait is resolved.
                for local_step in 0..definition.step_names.len() {
                    runtime.send_message_by_id(actor_id, local_step as u16, &[]);
                }

                let workflow = self.snapshot(runtime, &instance_id)?;
                Ok(WorkflowControlReply::Started { workflow })
            }
            WorkflowControlCommand::Inspect { instance_id } => {
                let workflow = self.snapshot(runtime, &instance_id)?;
                Ok(WorkflowControlReply::Inspected { workflow })
            }
            WorkflowControlCommand::Signal {
                instance_id,
                signal,
                payload,
            } => {
                let binding = self.live_binding(&instance_id)?.clone();
                let payload = match payload {
                    JsonValue::Null => None,
                    JsonValue::String(value) => Some(value),
                    _ => {
                        return Err(protocol_error(
                            WorkflowControlErrorCode::InvalidRequest,
                            "workflow signal payload must be null or a string in v0alpha1",
                            false,
                        ))
                    }
                };
                if !runtime.actors.contains_key(&binding.actor_id) {
                    return Err(protocol_error(
                        WorkflowControlErrorCode::NotFound,
                        "workflow runtime actor is not resident",
                        false,
                    ));
                }
                runtime.signal_workflow(binding.actor_id, &signal, payload);
                let workflow = self.snapshot(runtime, &instance_id)?;
                Ok(WorkflowControlReply::Signaled { workflow })
            }
            WorkflowControlCommand::Cancel {
                instance_id,
                reason: _,
            } => {
                let actor_id = self.live_binding(&instance_id)?.actor_id;
                if runtime.actors.contains_key(&actor_id) {
                    runtime.exit_actor(actor_id, ExitReason::Normal);
                }
                if let Some(binding) = self.instances.get_mut(&instance_id) {
                    binding.cancelled = true;
                }
                let workflow = self.snapshot(runtime, &instance_id)?;
                Ok(WorkflowControlReply::Cancelled { workflow })
            }
            WorkflowControlCommand::Query {
                instance_id,
                query,
                args,
            } => {
                if !args.is_empty() {
                    return Err(protocol_error(
                        WorkflowControlErrorCode::Unsupported,
                        "workflow query arguments are not supported by the runtime yet",
                        false,
                    ));
                }
                let binding = self.live_binding(&instance_id)?.clone();
                let result = runtime
                    .query_workflow(binding.actor_id, &query)
                    .ok_or_else(|| {
                        protocol_error(
                            WorkflowControlErrorCode::NotFound,
                            format!("workflow query not found: {query}"),
                            false,
                        )
                    })?;
                let value = runtime_value_to_json(runtime, binding.actor_id, result)?;
                Ok(WorkflowControlReply::QueryResult { value })
            }
        }
    }

    fn live_binding(
        &self,
        instance_id: &WorkflowInstanceId,
    ) -> Result<&InstanceBinding, WorkflowControlError> {
        let binding = self.instances.get(instance_id).ok_or_else(|| {
            protocol_error(
                WorkflowControlErrorCode::NotFound,
                format!("workflow instance not found: {instance_id}"),
                false,
            )
        })?;
        if binding.cancelled {
            return Err(protocol_error(
                WorkflowControlErrorCode::Conflict,
                "workflow instance is cancelled",
                false,
            ));
        }
        Ok(binding)
    }

    fn snapshot(
        &self,
        runtime: &Runtime,
        instance_id: &WorkflowInstanceId,
    ) -> Result<WorkflowSnapshot, WorkflowControlError> {
        let binding = self.instances.get(instance_id).ok_or_else(|| {
            protocol_error(
                WorkflowControlErrorCode::NotFound,
                format!("workflow instance not found: {instance_id}"),
                false,
            )
        })?;

        if binding.cancelled {
            return Ok(WorkflowSnapshot {
                instance_id: instance_id.clone(),
                definition_id: binding.definition_id.clone(),
                status: WorkflowLifecycleStatus::Cancelled,
                current_step: None,
                waiting_on: None,
                output: None,
                failure: None,
            });
        }

        let actor = runtime.actors.get(&binding.actor_id).ok_or_else(|| {
            protocol_error(
                WorkflowControlErrorCode::NotFound,
                "workflow runtime actor is not resident",
                false,
            )
        })?;
        let events = runtime.persistence.read_workflow_events(binding.actor_id);

        if let Some((step_name, message)) = events.iter().rev().find_map(|event| match event {
            super::WorkflowEvent::StepFailed {
                step_name, error, ..
            } => Some((step_name.clone(), error.clone())),
            _ => None,
        }) {
            return Ok(WorkflowSnapshot {
                instance_id: instance_id.clone(),
                definition_id: binding.definition_id.clone(),
                status: WorkflowLifecycleStatus::Failed,
                current_step: Some(step_name),
                waiting_on: None,
                output: None,
                failure: Some(WorkflowFailure {
                    code: "step_failed".into(),
                    message,
                    retryable: false,
                }),
            });
        }

        let completed_steps = events
            .iter()
            .filter(|event| matches!(event, super::WorkflowEvent::StepCompleted { .. }))
            .count();

        if completed_steps >= binding.step_names.len() {
            return Ok(WorkflowSnapshot {
                instance_id: instance_id.clone(),
                definition_id: binding.definition_id.clone(),
                status: WorkflowLifecycleStatus::Completed,
                current_step: None,
                waiting_on: None,
                output: None,
                failure: None,
            });
        }

        let suspended = actor.suspended_execution.is_some();
        let waiting_signal = actor.waiting_signal.as_deref();
        let waiting_on = waiting_signal
            .filter(|name| *name != super::LLM_SUSPEND_MARKER)
            .map(|name| WorkflowWait::Signal {
                name: name.to_owned(),
            });

        Ok(WorkflowSnapshot {
            instance_id: instance_id.clone(),
            definition_id: binding.definition_id.clone(),
            status: if suspended || waiting_signal.is_some() {
                WorkflowLifecycleStatus::Waiting
            } else {
                WorkflowLifecycleStatus::Running
            },
            current_step: binding.step_names.get(completed_steps).cloned(),
            waiting_on,
            output: None,
            failure: None,
        })
    }
}

fn protocol_error(
    code: WorkflowControlErrorCode,
    message: impl Into<String>,
    retryable: bool,
) -> WorkflowControlError {
    WorkflowControlError {
        code,
        message: message.into(),
        retryable,
    }
}

fn short_behavior_name(name: &str) -> String {
    name.rsplit('.').next().unwrap_or(name).to_owned()
}

fn runtime_value_to_json(
    runtime: &Runtime,
    actor_id: u64,
    value: Value,
) -> Result<JsonValue, WorkflowControlError> {
    if let Some(value) = value.as_int() {
        return Ok(JsonValue::from(value));
    }
    if let Some(value) = value.as_float() {
        return serde_json::Number::from_f64(value)
            .map(JsonValue::Number)
            .ok_or_else(|| {
                protocol_error(
                    WorkflowControlErrorCode::Internal,
                    "workflow query returned a non-finite float",
                    false,
                )
            });
    }
    if let Some(value) = value.as_bool() {
        return Ok(JsonValue::Bool(value));
    }
    if value.is_nil() || value.is_unit() {
        return Ok(JsonValue::Null);
    }
    if value.is_string() || value.is_ptr() {
        let constants = runtime
            .actors
            .get(&actor_id)
            .and_then(|actor| actor.bytecode_module.as_ref())
            .map(|module| module.constants.as_slice())
            .unwrap_or(&[]);
        return Ok(JsonValue::String(crate::vm::resolve_value_string(
            constants, value,
        )));
    }

    Err(protocol_error(
        WorkflowControlErrorCode::Unsupported,
        "workflow query returned a value that is not representable in the control protocol",
        false,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::Lexer;
    use crate::parser::Parser;
    use crate::typechecker::TypeChecker;
    use nulang_workflow_protocol::{
        WorkflowControlCommand, WorkflowControlErrorCode, WorkflowControlReply,
        WorkflowControlRequest, WorkflowControlResult, WorkflowDefinitionId,
        WorkflowInstanceId, WorkflowLifecycleStatus, WorkflowWait,
    };
    use serde_json::Value as JsonValue;
    use std::collections::BTreeMap;

    fn compile_workflow(source: &str) -> crate::bytecode::CodeModule {
        let mut lexer = Lexer::new(source);
        let tokens = lexer.lex().unwrap();
        let mut parser = Parser::new(tokens);
        let ast = parser.parse_module().unwrap();

        let mut type_checker = TypeChecker::new();
        type_checker.check_module(&ast).unwrap();

        let hir = crate::hir_lower::lower_module(&ast, &type_checker.inferred_decl_types);
        let mut mir = crate::mir_lower::lower_module(&hir).unwrap();
        crate::mir_codegen::compile_mir(&mut mir, "workflow-control-test").unwrap()
    }

    fn start_request(definition: &str, instance: &str) -> WorkflowControlRequest {
        WorkflowControlRequest::new(
            "req-start",
            WorkflowControlCommand::Start {
                definition_id: WorkflowDefinitionId::new(definition),
                instance_id: Some(WorkflowInstanceId::new(instance)),
                input: BTreeMap::new(),
                idempotency_key: Some(format!("start-{instance}")),
            },
        )
    }

    #[test]
    fn test_control_host_rejects_non_workflow_definition() {
        let mut host = WorkflowControlHost::new();
        let module = crate::bytecode::CodeModule::new("plain");

        let error = host
            .register_definition(WorkflowDefinitionId::new("plain"), module)
            .unwrap_err();

        assert_eq!(error.code, WorkflowControlErrorCode::InvalidRequest);
    }

    #[test]
    fn test_control_host_start_enqueues_all_workflow_steps() {
        let module = compile_workflow(
            r#"
            workflow Order {
                step reserve { 1 }
                step charge { 2 }
            }
            "#,
        );
        let mut host = WorkflowControlHost::new();
        host.register_definition(WorkflowDefinitionId::new("orders"), module)
            .unwrap();
        let mut runtime = Runtime::new();

        let response = host.handle(&mut runtime, start_request("orders", "wf-1"));

        let WorkflowControlResult::Ok {
            reply: WorkflowControlReply::Started { workflow },
        } = response.result
        else {
            panic!("start should succeed: {:?}", response.result);
        };
        let actor_id = host.actor_id_for(&WorkflowInstanceId::new("wf-1")).unwrap();
        assert_eq!(workflow.status, WorkflowLifecycleStatus::Running);
        assert_eq!(runtime.actors[&actor_id].mailbox.len(), 2);
    }

    #[test]
    fn test_control_host_completed_status_comes_from_runtime_events() {
        let module = compile_workflow(
            r#"
            workflow Order {
                step reserve { 1 }
                step charge { 2 }
            }
            "#,
        );
        let mut host = WorkflowControlHost::new();
        host.register_definition(WorkflowDefinitionId::new("orders"), module)
            .unwrap();
        let mut runtime = Runtime::new();

        host.handle(&mut runtime, start_request("orders", "wf-1"));
        runtime.run_scheduler();

        let response = host.handle(
            &mut runtime,
            WorkflowControlRequest::new(
                "req-inspect",
                WorkflowControlCommand::Inspect {
                    instance_id: WorkflowInstanceId::new("wf-1"),
                },
            ),
        );
        let WorkflowControlResult::Ok {
            reply: WorkflowControlReply::Inspected { workflow },
        } = response.result
        else {
            panic!("inspect should succeed: {:?}", response.result);
        };
        assert_eq!(workflow.status, WorkflowLifecycleStatus::Completed);
        assert_eq!(workflow.current_step, None);
    }

    #[test]
    fn test_control_host_signal_resumes_runtime_owned_workflow() {
        let module = compile_workflow(
            r#"
            workflow Approval {
                step wait {
                    perform Signal.wait("approved")
                }
                step done { 1 }
            }
            "#,
        );
        let mut host = WorkflowControlHost::new();
        host.register_definition(WorkflowDefinitionId::new("approval"), module)
            .unwrap();
        let mut runtime = Runtime::new();

        host.handle(&mut runtime, start_request("approval", "wf-signal"));
        runtime.run_scheduler();

        let waiting = host.handle(
            &mut runtime,
            WorkflowControlRequest::new(
                "req-waiting",
                WorkflowControlCommand::Inspect {
                    instance_id: WorkflowInstanceId::new("wf-signal"),
                },
            ),
        );
        let WorkflowControlResult::Ok {
            reply: WorkflowControlReply::Inspected { workflow },
        } = waiting.result
        else {
            panic!("inspect should succeed: {:?}", waiting.result);
        };
        assert_eq!(workflow.status, WorkflowLifecycleStatus::Waiting);
        assert_eq!(
            workflow.waiting_on,
            Some(WorkflowWait::Signal {
                name: "approved".into()
            })
        );

        let signaled = host.handle(
            &mut runtime,
            WorkflowControlRequest::new(
                "req-signal",
                WorkflowControlCommand::Signal {
                    instance_id: WorkflowInstanceId::new("wf-signal"),
                    signal: "approved".into(),
                    payload: JsonValue::Null,
                },
            ),
        );
        assert!(matches!(signaled.result, WorkflowControlResult::Ok { .. }));

        runtime.run_scheduler();
        let completed = host.handle(
            &mut runtime,
            WorkflowControlRequest::new(
                "req-complete",
                WorkflowControlCommand::Inspect {
                    instance_id: WorkflowInstanceId::new("wf-signal"),
                },
            ),
        );
        let WorkflowControlResult::Ok {
            reply: WorkflowControlReply::Inspected { workflow },
        } = completed.result
        else {
            panic!("inspect should succeed: {:?}", completed.result);
        };
        assert_eq!(workflow.status, WorkflowLifecycleStatus::Completed);
    }

    #[test]
    fn test_control_host_rejects_nonempty_start_input_until_runtime_mapping_exists() {
        let module = compile_workflow("workflow W { step only { 1 } }");
        let mut host = WorkflowControlHost::new();
        host.register_definition(WorkflowDefinitionId::new("w"), module)
            .unwrap();
        let mut runtime = Runtime::new();
        let mut input = BTreeMap::new();
        input.insert("customer_id".into(), JsonValue::String("c-1".into()));

        let response = host.handle(
            &mut runtime,
            WorkflowControlRequest::new(
                "req-input",
                WorkflowControlCommand::Start {
                    definition_id: WorkflowDefinitionId::new("w"),
                    instance_id: Some(WorkflowInstanceId::new("wf-input")),
                    input,
                    idempotency_key: None,
                },
            ),
        );

        let WorkflowControlResult::Error { error } = response.result else {
            panic!("unsupported input must fail");
        };
        assert_eq!(error.code, WorkflowControlErrorCode::InvalidRequest);
    }
}
