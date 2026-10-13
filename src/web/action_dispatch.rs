//! Renderer-neutral UI action resolution for Web hosts.
//!
//! Only document-bound, compiler-described scalar arguments may be decoded.
//! Binding never invokes a parameterized handler or authorizes external effects;
//! hosts must enforce session authority and idempotency before execution.

use crate::bytecode::{CodeModule, Constant};
use crate::web::contracts::HandlerParamContract;
use crate::vm::{Value, VM};
use nulang_ui_protocol::{ActionPlacement, HostToRuntimeMessage, Revision, UiDocument, WireValue};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiActionDispatchError {
    InvalidEnvelope(String),
    InvalidDocument(String),
    DocumentMismatch,
    RevisionMismatch {
        expected: Revision,
        found: Revision,
    },
    UnregisteredAction(String),
    UnexpectedPayload,
    InvalidPayload(String),
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
            Self::InvalidDocument(message) => {
                write!(f, "invalid authoritative UI document: {message}")
            }
            Self::DocumentMismatch => f.write_str("UI action targets another document"),
            Self::RevisionMismatch { expected, found } => write!(
                f,
                "UI action revision mismatch: expected {}, found {}",
                expected.0, found.0
            ),
            Self::UnregisteredAction(action) => write!(
                f,
                "UI action '{action}' is not registered as a server action on this document"
            ),
            Self::UnexpectedPayload => f.write_str(
                "zero-argument UI action received data without a compiler-owned binding plan"
            ),
            Self::InvalidPayload(message) => write!(f, "invalid UI action payload: {message}"),
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

/// Shared document authorization preflight. A session host must supply a
/// server-owned document and authenticate the session before calling this.
fn authorize_document_action(
    document: &UiDocument,
    message: &HostToRuntimeMessage,
) -> Result<(), UiActionDispatchError> {
    document
        .validate()
        .map_err(|error| UiActionDispatchError::InvalidDocument(error.to_string()))?;
    message
        .validate()
        .map_err(|error| UiActionDispatchError::InvalidEnvelope(error.to_string()))?;

    let HostToRuntimeMessage::InvokeAction { request, .. } = message;
    if request.document_id != document.document_id {
        return Err(UiActionDispatchError::DocumentMismatch);
    }
    if request.revision != document.revision {
        return Err(UiActionDispatchError::RevisionMismatch {
            expected: document.revision,
            found: request.revision,
        });
    }
    if request.placement != ActionPlacement::Server {
        return Err(UiActionDispatchError::ClientPlacement);
    }
    let registered = document.nodes.iter().any(|node| {
        node.actions.iter().any(|binding| {
            binding.action_id == request.action_id && binding.placement == ActionPlacement::Server
        })
    });
    if !registered {
        return Err(UiActionDispatchError::UnregisteredAction(
            request.action_id.as_str().to_owned(),
        ));
    }
    Ok(())
}

/// A validated, non-executing action binding. Constants must still cross the
/// compiler-owned runtime binding ABI before execution; this type grants no
/// authentication, effect authority, or idempotent execution privileges.
#[derive(Debug, Clone, PartialEq)]
pub struct BoundUiAction {
    pub action_id: String,
    pub code_offset: usize,
    pub arguments: Vec<Constant>,
}

/// Bind primitive action payloads only after checking the server-owned UI
/// document and the compiler's declared function arity. The caller must pass
/// compiler-owned parameter metadata, not a client-provided binding plan.
///
/// This stage deliberately does not invoke the handler; the session host still
/// has to authenticate the principal, authorize the action, and deduplicate
/// idempotency keys before scheduling effects.
pub fn bind_document_action_payload(
    module: &CodeModule,
    document: &UiDocument,
    message: &HostToRuntimeMessage,
    params: &[HandlerParamContract],
) -> Result<BoundUiAction, UiActionDispatchError> {
    authorize_document_action(document, message)?;
    let HostToRuntimeMessage::InvokeAction { request, .. } = message;
    let action_id = request.action_id.as_str();
    let Some(info) = module
        .debug_functions
        .iter()
        .find(|function| function.name == action_id)
    else {
        return Err(UiActionDispatchError::UnknownAction(action_id.to_owned()));
    };
    if !module
        .function_table
        .iter()
        .any(|offset| *offset == info.code_offset)
    {
        return Err(UiActionDispatchError::UnknownAction(action_id.to_owned()));
    }
    if params.len() != info.params.len() {
        return Err(UiActionDispatchError::RequiresBindingPlan {
            action_id: action_id.to_owned(),
            parameter_count: info.params.len(),
        });
    }

    let arguments = if params.is_empty() {
        if !matches!(&request.payload, WireValue::Null)
            && !matches!(&request.payload, WireValue::Object(fields) if fields.is_empty())
        {
            return Err(UiActionDispatchError::UnexpectedPayload);
        }
        Vec::new()
    } else {
        let WireValue::Object(fields) = &request.payload else {
            return Err(UiActionDispatchError::InvalidPayload(
                "expected object payload for parameterized action".to_owned(),
            ));
        };
        // Neither duplicate parameter names nor extra payload keys may be
        // interpreted as an authorized field by matching only the field count.
        let mut parameter_names = std::collections::BTreeSet::new();
        for param in params {
            if !parameter_names.insert(param.name.as_str()) {
                return Err(UiActionDispatchError::InvalidPayload(
                    "duplicate compiler parameter name".to_owned(),
                ));
            }
        }
        if fields.len() != params.len()
            || fields
                .keys()
                .any(|name| !parameter_names.contains(name.as_str()))
        {
            return Err(UiActionDispatchError::InvalidPayload(
                "payload fields do not match compiler parameter names".to_owned(),
            ));
        }
        let mut arguments = Vec::with_capacity(params.len());
        for param in params {
            if param.name.trim().is_empty()
                || param.capability.is_some()
                || param.request.is_some()
            {
                return Err(UiActionDispatchError::InvalidPayload(
                    "unsupported or privileged compiler parameter contract".to_owned(),
                ));
            }
            let Some(ty) = param.ty.as_deref() else {
                return Err(UiActionDispatchError::InvalidPayload(format!(
                    "parameter '{}' has no proven type",
                    param.name
                )));
            };
            let Some(value) = fields.get(&param.name) else {
                return Err(UiActionDispatchError::InvalidPayload(format!(
                    "missing parameter '{}'",
                    param.name
                )));
            };
            arguments.push(decode_action_scalar(value, ty).ok_or_else(|| {
                UiActionDispatchError::InvalidPayload(format!(
                    "parameter '{}' is not a valid {} scalar",
                    param.name, ty
                ))
            })?);
        }
        arguments
    };

    Ok(BoundUiAction {
        action_id: action_id.to_owned(),
        code_offset: info.code_offset,
        arguments,
    })
}

fn decode_action_scalar(value: &WireValue, ty: &str) -> Option<Constant> {
    match (ty.trim(), value) {
        ("String", WireValue::String(value)) => Some(Constant::String(value.clone())),
        ("Int", WireValue::I64(value)) => Some(Constant::Int(value.0)),
        ("Int", WireValue::String(value)) => value.parse::<i64>().ok().map(Constant::Int),
        ("Bool", WireValue::Bool(value)) => Some(Constant::Bool(*value)),
        ("Bool", WireValue::String(value)) if value == "true" => {
            Some(Constant::Bool(true))
        }
        ("Bool", WireValue::String(value)) if value == "false" => {
            Some(Constant::Bool(false))
        }
        ("Float", WireValue::F64(value)) => {
            let float = value.to_f64();
            float.is_finite().then_some(Constant::Float(float))
        }
        ("Float", WireValue::String(value)) => value
            .parse::<f64>()
            .ok()
            .filter(|parsed| parsed.is_finite())
            .map(Constant::Float),
        _ => None,
    }
}

/// Resolve a zero-argument server action against a *trusted, server-owned* UI
/// document. A client-supplied document must never be used as the authority.
/// This is a dispatch building block; it does not provide authentication,
/// authorization or idempotent execution on its own.
pub fn resolve_document_zero_arg_action(
    module: &CodeModule,
    document: &UiDocument,
    message: &HostToRuntimeMessage,
) -> Result<ResolvedUiAction, UiActionDispatchError> {
    authorize_document_action(document, message)?;

    let HostToRuntimeMessage::InvokeAction { request, .. } = message;
    // A zero-argument action has no defined payload decoder. Do not silently
    // discard client-supplied fields, or reinterpret them as VM registers.
    let payload_empty = match &request.payload {
        WireValue::Null => true,
        WireValue::Object(fields) => fields.is_empty(),
        _ => false,
    };
    if !payload_empty {
        return Err(UiActionDispatchError::UnexpectedPayload);
    }
    resolve_zero_arg_action(module, message)
}

/// Execute only after binding the action to the current, authoritative UI
/// document. The host must authenticate the caller and deduplicate action
/// idempotency keys before invoking effects with external side effects.
pub fn invoke_document_zero_arg_action(
    module: &CodeModule,
    document: &UiDocument,
    message: &HostToRuntimeMessage,
) -> Result<Value, UiActionDispatchError> {
    let resolved = resolve_document_zero_arg_action(module, document, message)?;
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
        ActionBinding, ActionRequest, CorrelationId, DocumentId, IdempotencyKey, Revision,
        UiDocument, UiNode, WireValue,
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

    fn trusted_document(action: &str, placement: ActionPlacement) -> UiDocument {
        let mut root = UiNode::new("root", "interaction_root");
        root.actions.push(ActionBinding {
            event: "activate".to_owned(),
            action_id: action.into(),
            placement,
        });
        UiDocument::new("app", Revision(1), "root", vec![root])
    }

    fn typed_param(name: &str, ty: &str) -> crate::web::contracts::HandlerParamContract {
        crate::web::contracts::HandlerParamContract {
            name: name.to_owned(),
            ty: Some(ty.to_owned()),
            capability: None,
            request: None,
        }
    }

    fn action_payload(fields: &[(&str, WireValue)]) -> HostToRuntimeMessage {
        let mut message = action_message("save", ActionPlacement::Server);
        let HostToRuntimeMessage::InvokeAction { request, .. } = &mut message;
        request.payload = WireValue::Object(
            fields
                .iter()
                .map(|(name, value)| ((*name).to_owned(), value.clone()))
                .collect(),
        );
        message
    }

    #[test]
    fn document_bound_scalar_payload_binds_by_compiler_parameter_order() {
        let module = module_with_action("save", vec![0, 1, 2]);
        let document = trusted_document("save", ActionPlacement::Server);
        let message = action_payload(&[
            ("count", WireValue::from("42")),
            ("active", WireValue::Bool(true)),
            ("title", WireValue::from("Save me")),
        ]);
        let params = [
            typed_param("title", "String"),
            typed_param("count", "Int"),
            typed_param("active", "Bool"),
        ];

        let bound = bind_document_action_payload(&module, &document, &message, &params)
            .expect("trusted document and exact compiler arity permit scalar binding");
        assert_eq!(bound.action_id, "save");
        assert_eq!(
            bound.arguments,
            vec![
                Constant::String("Save me".into()),
                Constant::Int(42),
                Constant::Bool(true),
            ]
        );
    }

    #[test]
    fn document_bound_scalar_payload_rejects_unknown_missing_and_wrong_types() {
        let module = module_with_action("save", vec![0, 1]);
        let document = trusted_document("save", ActionPlacement::Server);
        let params = [typed_param("title", "String"), typed_param("count", "Int")];

        for message in [
            action_payload(&[("title", WireValue::from("one"))]),
            action_payload(&[
                ("title", WireValue::from("one")),
                ("count", WireValue::I64(1.into())),
                ("admin", WireValue::Bool(true)),
            ]),
            action_payload(&[
                ("title", WireValue::from("one")),
                ("count", WireValue::from("not-an-int")),
            ]),
        ] {
            assert!(matches!(
                bind_document_action_payload(&module, &document, &message, &params),
                Err(UiActionDispatchError::InvalidPayload(_))
            ));
        }
    }

    #[test]
    fn document_bound_scalar_payload_rejects_unproven_compiler_contracts() {
        let module = module_with_action("save", vec![0, 1]);
        let document = trusted_document("save", ActionPlacement::Server);
        let message = action_payload(&[("title", WireValue::from("one"))]);

        assert!(matches!(
            bind_document_action_payload(
                &module,
                &document,
                &message,
                &[typed_param("title", "String")]
            ),
            Err(UiActionDispatchError::RequiresBindingPlan { .. })
        ));

        let mut privileged = typed_param("title", "String");
        privileged.capability = Some("iso".into());
        assert!(matches!(
            bind_document_action_payload(
                &module_with_action("save", vec![0]),
                &document,
                &message,
                &[privileged]
            ),
            Err(UiActionDispatchError::InvalidPayload(_))
        ));
    }

    #[test]
    fn document_bound_scalar_payload_rejects_duplicate_contract_parameter_names() {
        let module = module_with_action("save", vec![0, 1]);
        let document = trusted_document("save", ActionPlacement::Server);
        let message = action_payload(&[
            ("title", WireValue::from("value")),
            ("admin", WireValue::Bool(true)),
        ]);
        let params = [typed_param("title", "String"), typed_param("title", "String")];

        assert!(matches!(
            bind_document_action_payload(&module, &document, &message, &params),
            Err(UiActionDispatchError::InvalidPayload(_))
        ));
    }

    #[test]
    fn document_bound_scalar_payload_rejects_nonfinite_float() {
        let module = module_with_action("save", vec![0]);
        let document = trusted_document("save", ActionPlacement::Server);
        let params = [typed_param("ratio", "Float")];
        let message = action_payload(&[(
            "ratio",
            WireValue::F64(nulang_ui_protocol::F64Bits::from_f64(f64::NAN)),
        )]);

        assert!(matches!(
            bind_document_action_payload(&module, &document, &message, &params),
            Err(UiActionDispatchError::InvalidPayload(_))
        ));
    }

    #[test]
    fn document_bound_scalar_payload_rejects_foreign_or_unregistered_actions() {
        let module = module_with_action("save", vec![0]);
        let document = trusted_document("save", ActionPlacement::Server);
        let message = action_payload(&[("title", WireValue::from("one"))]);
        let params = [typed_param("title", "String")];

        assert!(matches!(
            bind_document_action_payload(
                &module,
                &trusted_document("other", ActionPlacement::Server),
                &message,
                &params
            ),
            Err(UiActionDispatchError::UnregisteredAction(_))
        ));

        let mut stale = message.clone();
        let HostToRuntimeMessage::InvokeAction { request, .. } = &mut stale;
        request.revision = Revision(999);
        assert!(matches!(
            bind_document_action_payload(&module, &document, &stale, &params),
            Err(UiActionDispatchError::RevisionMismatch { .. })
        ));
    }

    #[test]
    fn document_bound_action_executes_only_a_registered_server_handler() {
        let module = module_with_action("save", Vec::new());
        let document = trusted_document("save", ActionPlacement::Server);
        let message = action_message("save", ActionPlacement::Server);

        let resolved = resolve_document_zero_arg_action(&module, &document, &message)
            .expect("trusted document registers the server handler");
        assert_eq!(resolved.action_id, "save");

        let value = invoke_document_zero_arg_action(&module, &document, &message)
            .expect("registered zero-argument handler executes");
        assert!(value.is_unit());
    }

    #[test]
    fn document_bound_action_rejects_unregistered_and_client_only_bindings() {
        let module = module_with_action("save", Vec::new());
        let message = action_message("save", ActionPlacement::Server);

        let document = trusted_document("other", ActionPlacement::Server);
        assert_eq!(
            resolve_document_zero_arg_action(&module, &document, &message),
            Err(UiActionDispatchError::UnregisteredAction("save".to_owned()))
        );

        let document = trusted_document("save", ActionPlacement::Client);
        assert_eq!(
            resolve_document_zero_arg_action(&module, &document, &message),
            Err(UiActionDispatchError::UnregisteredAction("save".to_owned()))
        );
    }

    #[test]
    fn document_bound_action_rejects_stale_and_foreign_documents() {
        let module = module_with_action("save", Vec::new());
        let document = trusted_document("save", ActionPlacement::Server);

        let mut stale = action_message("save", ActionPlacement::Server);
        let HostToRuntimeMessage::InvokeAction { request, .. } = &mut stale;
        request.revision = Revision(0);
        assert_eq!(
            resolve_document_zero_arg_action(&module, &document, &stale),
            Err(UiActionDispatchError::RevisionMismatch {
                expected: Revision(1),
                found: Revision(0),
            })
        );

        let mut foreign = action_message("save", ActionPlacement::Server);
        let HostToRuntimeMessage::InvokeAction { request, .. } = &mut foreign;
        request.document_id = "foreign".into();
        assert_eq!(
            resolve_document_zero_arg_action(&module, &document, &foreign),
            Err(UiActionDispatchError::DocumentMismatch)
        );
    }

    #[test]
    fn document_bound_action_rejects_payload_without_an_argument_contract() {
        let module = module_with_action("save", Vec::new());
        let document = trusted_document("save", ActionPlacement::Server);
        let mut message = action_message("save", ActionPlacement::Server);
        let HostToRuntimeMessage::InvokeAction { request, .. } = &mut message;
        request.payload = WireValue::from("unexpected");

        assert_eq!(
            resolve_document_zero_arg_action(&module, &document, &message),
            Err(UiActionDispatchError::UnexpectedPayload)
        );
    }

    #[test]
    fn document_bound_action_rejects_invalid_authoritative_document() {
        let module = module_with_action("save", Vec::new());
        let mut document = trusted_document("save", ActionPlacement::Server);
        document.root = "missing".into();
        let result = resolve_document_zero_arg_action(
            &module,
            &document,
            &action_message("save", ActionPlacement::Server),
        );
        assert!(matches!(
            result,
            Err(UiActionDispatchError::InvalidDocument(_))
        ));
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
