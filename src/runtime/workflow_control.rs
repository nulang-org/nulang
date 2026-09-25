//! Runtime host for the versioned workflow control protocol.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::Lexer;
    use crate::parser::Parser;
    use crate::typechecker::TypeChecker;
    use nulang_workflow_protocol::{
        WorkflowControlCommand, WorkflowControlReply, WorkflowControlRequest,
        WorkflowControlResult, WorkflowDefinitionId, WorkflowInstanceId,
        WorkflowLifecycleStatus, WorkflowWait,
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
