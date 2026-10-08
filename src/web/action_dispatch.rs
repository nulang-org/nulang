//! Renderer-neutral UI action resolution for Web hosts.
//!
//! This layer deliberately stops before parameter binding. A protocol action may
//! be invoked directly only when compiler bytecode metadata proves the target is
//! a top-level zero-argument function. Parameterized handlers must wait for a
//! compiler-owned action binding plan rather than guessing from transport data.

use crate::bytecode::CodeModule;
use crate::vm::{Value, VM};
use nulang_ui_protocol::{ActionPlacement, HostToRuntimeMessage};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiActionDispatchError {
    InvalidEnvelope(String),
    ClientPlacement,
    UnknownAction(String),
    RequiresBindingPlan {
        action_id: String,
        parameter_count: usize,
    },
    Execution(String),
}

impl std::fmt::Display for UiActionDispatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidEnvelope(message) => write!(f, "invalid UI action envelope: {message}"),
            Self::ClientPlacement => {
                f.write_str("client-placement action cannot be executed by the server runtime")
            }
            Self::UnknownAction(action_id) => write!(f, "unknown UI action '{action_id}'"),
            Self::RequiresBindingPlan {
                action_id,
                parameter_count,
            } => write!(
                f,
                "UI action '{action_id}' has {parameter_count} parameter(s) and requires a compiler-owned binding plan"
            ),
            Self::Execution(message) => write!(f, "UI action execution failed: {message}"),
        }
    }
}

impl std::error::Error for UiActionDispatchError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedUiAction {
    pub action_id: String,
    pub code_offset: usize,
}

pub fn resolve_zero_arg_action(
    module: &CodeModule,
    message: &HostToRuntimeMessage,
) -> Result<ResolvedUiAction, UiActionDispatchError> {
    message
        .validate()
        .map_err(|error| UiActionDispatchError::InvalidEnvelope(error.to_string()))?;

    let request = match message {
        HostToRuntimeMessage::InvokeAction { request, .. } => request,
    };
    if request.placement != ActionPlacement::Server {
        return Err(UiActionDispatchError::ClientPlacement);
    }

    let action_id = request.action_id.as_str();
    let Some(info) = module
        .debug_functions
        .iter()
        .find(|function| function.name == action_id)
    else {
        return Err(UiActionDispatchError::UnknownAction(action_id.to_string()));
    };
    if !module
        .function_table
        .iter()
        .any(|offset| *offset == info.code_offset)
    {
        return Err(UiActionDispatchError::UnknownAction(action_id.to_string()));
    }
    if !info.params.is_empty() {
        return Err(UiActionDispatchError::RequiresBindingPlan {
            action_id: action_id.to_string(),
            parameter_count: info.params.len(),
        });
    }

    Ok(ResolvedUiAction {
        action_id: action_id.to_string(),
        code_offset: info.code_offset,
    })
}

pub fn invoke_zero_arg_action(
    module: &CodeModule,
    message: &HostToRuntimeMessage,
) -> Result<Value, UiActionDispatchError> {
    let resolved = resolve_zero_arg_action(module, message)?;
    let mut vm = VM::new();
    vm.load_module(module.clone());
    vm.call_function(0, resolved.code_offset, &[])
        .map_err(|error| UiActionDispatchError::Execution(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bytecode::{Constant, DebugFunctionInfo, Instruction, OpCode};
    use nulang_ui_protocol::{
        ActionRequest, CorrelationId, DocumentId, IdempotencyKey, Revision, WireValue,
    };

    fn module_with_action(name: &str, params: Vec<usize>) -> CodeModule {
        let mut module = CodeModule::new("actions");
        let offset = module.instructions.len();
        let unit_index = module.add_constant(Constant::Unit);
        module.emit(Instruction::new3(
            OpCode::ConstU,
            ((unit_index >> 8) & 0xFF) as u8,
            (unit_index & 0xFF) as u8,
            0,
        ));
        module.emit(Instruction::new1(OpCode::RetVal, 0));
        module.function_table.push(offset);
        module.function_local_counts.push(params.len());
        module.debug_functions.push(DebugFunctionInfo {
            name: name.to_string(),
            code_offset: offset,
            code_len: 2,
            params,
            locals: Vec::new(),
        });
        module
    }

    fn action_message(name: &str, placement: ActionPlacement) -> HostToRuntimeMessage {
        HostToRuntimeMessage::invoke_action(ActionRequest {
            document_id: DocumentId::from("app"),
            revision: Revision(1),
            action_id: name.into(),
            placement,
            correlation_id: CorrelationId::from("corr-1"),
            idempotency_key: IdempotencyKey::from("idem-1"),
            payload: WireValue::Null,
        })
    }

    #[test]
    fn resolves_server_zero_arg_action_from_compiler_metadata() {
        let module = module_with_action("save", Vec::new());
        let resolved =
            resolve_zero_arg_action(&module, &action_message("save", ActionPlacement::Server))
                .expect("zero-arg server action should resolve");

        assert_eq!(resolved.action_id, "save");
        assert_eq!(resolved.code_offset, 0);
    }

    #[test]
    fn parameterized_action_requires_compiler_owned_binding_plan() {
        let module = module_with_action("save", vec![0, 1]);
        let error =
            resolve_zero_arg_action(&module, &action_message("save", ActionPlacement::Server))
                .expect_err("parameterized action must not guess bindings");

        assert_eq!(
            error,
            UiActionDispatchError::RequiresBindingPlan {
                action_id: "save".to_string(),
                parameter_count: 2,
            }
        );
    }

    #[test]
    fn client_action_cannot_cross_server_dispatch_boundary() {
        let module = module_with_action("save", Vec::new());
        let error =
            resolve_zero_arg_action(&module, &action_message("save", ActionPlacement::Client))
                .expect_err("client placement must fail closed on server");

        assert_eq!(error, UiActionDispatchError::ClientPlacement);
    }

    #[test]
    fn invokes_zero_arg_action_through_vm_call_boundary() {
        let module = module_with_action("save", Vec::new());
        let value =
            invoke_zero_arg_action(&module, &action_message("save", ActionPlacement::Server))
                .expect("zero-arg action should execute");

        assert!(value.is_unit());
    }
}
