//! Renderer-neutral UI action resolution for Web hosts.
//!
//! This layer deliberately stops before parameter binding. A protocol action may
//! be invoked directly only when compiler bytecode metadata proves the target is
//! a top-level zero-argument function. Parameterized handlers must wait for a
//! compiler-owned action binding plan rather than guessing from transport data.

use crate::bytecode::{CodeModule, Constant};
use crate::vm::{Value, VM};
use crate::web::contracts::HandlerParamContract;
use crate::web::runtime_bindings::decode_scalar_constant;
use nulang_ui_protocol::{ActionPlacement, HostToRuntimeMessage, WireValue};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiActionDispatchError {
    InvalidEnvelope(String),
    ClientPlacement,
    UnknownAction(String),
    RequiresBindingPlan {
        action_id: String,
        parameter_count: usize,
    },
    InvalidPayload(String),
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
            Self::InvalidPayload(message) => write!(f, "invalid UI action payload: {message}"),
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

#[derive(Debug, Clone, PartialEq)]
pub struct BoundActionArgument {
    pub handler_index: usize,
    pub value: Constant,
}

/// Bind a renderer-neutral action payload to compiler-owned handler slots.
///
/// Parameter names, order, and source types come from the compiler's existing
/// `HandlerParamContract`. String payloads use the same primitive decoder as
/// route bindings, while native hosts may send already-typed primitive
/// `WireValue` values. Complex values remain rejected until their runtime ABI
/// representation is compiler-defined.
pub fn bind_action_payload(
    params: &[HandlerParamContract],
    message: &HostToRuntimeMessage,
) -> Result<Vec<BoundActionArgument>, UiActionDispatchError> {
    message
        .validate()
        .map_err(|error| UiActionDispatchError::InvalidEnvelope(error.to_string()))?;

    let request = match message {
        HostToRuntimeMessage::InvokeAction { request, .. } => request,
    };
    if request.placement != ActionPlacement::Server {
        return Err(UiActionDispatchError::ClientPlacement);
    }

    if params.is_empty() {
        return Ok(Vec::new());
    }

    let WireValue::Object(fields) = &request.payload else {
        return Err(UiActionDispatchError::InvalidPayload(
            "parameterized UI action payload must be an object".to_string(),
        ));
    };

    params
        .iter()
        .enumerate()
        .map(|(handler_index, param)| {
            let value = fields.get(&param.name).ok_or_else(|| {
                UiActionDispatchError::InvalidPayload(format!(
                    "missing payload field '{}'",
                    param.name
                ))
            })?;
            let value = decode_action_constant(value, param.ty.as_deref()).map_err(|message| {
                UiActionDispatchError::InvalidPayload(format!(
                    "payload field '{}': {message}",
                    param.name
                ))
            })?;
            Ok(BoundActionArgument {
                handler_index,
                value,
            })
        })
        .collect()
}

fn decode_action_constant(value: &WireValue, ty: Option<&str>) -> Result<Constant, String> {
    match value {
        WireValue::String(raw) => decode_scalar_constant(raw, ty),
        WireValue::Bool(value) if matches!(ty.map(str::trim), None | Some("Bool")) => {
            Ok(Constant::Bool(*value))
        }
        WireValue::I64(value) if matches!(ty.map(str::trim), None | Some("Int")) => {
            Ok(Constant::Int(value.0))
        }
        WireValue::F64(value) if matches!(ty.map(str::trim), None | Some("Float")) => {
            let value = value.to_f64();
            value
                .is_finite()
                .then_some(Constant::Float(value))
                .ok_or_else(|| "expected finite Float".to_string())
        }
        WireValue::Bool(_) => Err(format!(
            "expected {}, got Bool",
            ty.unwrap_or("string-compatible value")
        )),
        WireValue::I64(_) => Err(format!(
            "expected {}, got Int",
            ty.unwrap_or("string-compatible value")
        )),
        WireValue::F64(_) => Err(format!(
            "expected {}, got Float",
            ty.unwrap_or("string-compatible value")
        )),
        WireValue::Null => Err("null cannot bind a required handler parameter".to_string()),
        WireValue::Bytes(_) | WireValue::Array(_) | WireValue::Object(_) => {
            Err("complex payload values do not yet have a compiler-defined VM ABI".to_string())
        }
    }
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
    use crate::bytecode::{DebugFunctionInfo, Instruction, OpCode};
    use nulang_ui_protocol::{
        ActionRequest, CorrelationId, DocumentId, IdempotencyKey, Revision, WireValue,
    };

    fn module_with_action(name: &str, params: Vec<usize>) -> CodeModule {
        let mut module = CodeModule::new("actions");
        let offset = module.emit(Instruction::new0(OpCode::Ret));
        module.function_table.push(offset);
        module.function_local_counts.push(params.len());
        module.debug_functions.push(DebugFunctionInfo {
            name: name.to_string(),
            code_offset: offset,
            code_len: 1,
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
    fn handler_param(name: &str, ty: &str) -> crate::web::contracts::HandlerParamContract {
        crate::web::contracts::HandlerParamContract {
            name: name.to_string(),
            ty: Some(ty.to_string()),
            capability: None,
            request: None,
        }
    }

    #[test]
    fn binds_action_payload_by_compiler_parameter_order() {
        let params = vec![
            handler_param("title", "String"),
            handler_param("count", "Int"),
            handler_param("active", "Bool"),
        ];
        let mut fields = std::collections::BTreeMap::new();
        fields.insert("active".to_string(), WireValue::Bool(true));
        fields.insert("count".to_string(), WireValue::String("42".to_string()));
        fields.insert(
            "title".to_string(),
            WireValue::String("Ship it".to_string()),
        );
        let message = HostToRuntimeMessage::invoke_action(ActionRequest {
            document_id: DocumentId::from("app"),
            revision: Revision(1),
            action_id: "save".into(),
            placement: ActionPlacement::Server,
            correlation_id: CorrelationId::from("corr-1"),
            idempotency_key: IdempotencyKey::from("idem-1"),
            payload: WireValue::Object(fields),
        });

        let args = bind_action_payload(&params, &message).expect("typed payload should bind");

        assert_eq!(args.len(), 3);
        assert_eq!(args[0].handler_index, 0);
        assert_eq!(
            args[0].value,
            crate::bytecode::Constant::String("Ship it".to_string())
        );
        assert_eq!(args[1].handler_index, 1);
        assert_eq!(args[1].value, crate::bytecode::Constant::Int(42));
        assert_eq!(args[2].handler_index, 2);
        assert_eq!(args[2].value, crate::bytecode::Constant::Bool(true));
    }

    #[test]
    fn action_payload_missing_required_field_fails_closed() {
        let params = vec![
            handler_param("title", "String"),
            handler_param("count", "Int"),
        ];
        let mut fields = std::collections::BTreeMap::new();
        fields.insert(
            "title".to_string(),
            WireValue::String("Only title".to_string()),
        );
        let message = HostToRuntimeMessage::invoke_action(ActionRequest {
            document_id: DocumentId::from("app"),
            revision: Revision(1),
            action_id: "save".into(),
            placement: ActionPlacement::Server,
            correlation_id: CorrelationId::from("corr-1"),
            idempotency_key: IdempotencyKey::from("idem-1"),
            payload: WireValue::Object(fields),
        });

        let error =
            bind_action_payload(&params, &message).expect_err("missing count must fail closed");
        assert!(error.to_string().contains("missing payload field 'count'"));
    }

    #[test]
    fn action_payload_must_be_an_object_for_parameterized_handlers() {
        let params = vec![handler_param("title", "String")];
        let message = HostToRuntimeMessage::invoke_action(ActionRequest {
            document_id: DocumentId::from("app"),
            revision: Revision(1),
            action_id: "save".into(),
            placement: ActionPlacement::Server,
            correlation_id: CorrelationId::from("corr-1"),
            idempotency_key: IdempotencyKey::from("idem-1"),
            payload: WireValue::String("not-an-object".to_string()),
        });

        let error =
            bind_action_payload(&params, &message).expect_err("non-object payload must fail");
        assert!(error
            .to_string()
            .contains("parameterized UI action payload must be an object"));
    }
}
