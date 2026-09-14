//! Interpreter-only execution for compiler-authorized native client actions.
//!
//! Native hosts send the canonical `nulang-ui-msg/1` `InvokeAction` envelope.
//! The runtime resolves only compiler-authorized opaque action IDs, executes the
//! allowlisted function-table entry in the interpreter, and accepts only a
//! canonical `RuntimeToHostMessage` for the same document/revision lineage.

use std::fmt;

use nulang_ui_protocol::{ActionRequest, RuntimeToHostMessage};

use crate::format::mobile_nbc::{MobileActionMetadata, MobileNbcArtifact, MobileNbcError};
use crate::mobile::action::{
    decode_client_action_invocation, decode_client_action_output, encode_client_action_invocation,
    ClientActionProtocolError,
};
use crate::vm::{Value, VM};

#[derive(Debug, Clone)]
pub struct ClientActionRuntime {
    module: crate::bytecode::CodeModule,
    actions: MobileActionMetadata,
}

#[derive(Debug)]
pub enum ClientActionRuntimeError {
    Artifact(MobileNbcError),
    Request(ClientActionProtocolError),
    UnauthorizedAction { action_id: String },
    InvalidFunctionIndex { action_id: String, function_index: u32 },
    Vm(String),
    NonStringResult { action_id: String },
    InvalidUtf8Result { action_id: String },
    Output(ClientActionProtocolError),
    OutputDocumentMismatch { expected: String, found: String },
    PatchBaseRevisionMismatch { expected: u64, found: u64 },
}

impl fmt::Display for ClientActionRuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Artifact(error) => write!(f, "invalid mobile artifact: {error}"),
            Self::Request(error) => write!(f, "invalid client action request: {error}"),
            Self::UnauthorizedAction { action_id } => {
                write!(f, "client action '{action_id}' is not authorized by this artifact")
            }
            Self::InvalidFunctionIndex {
                action_id,
                function_index,
            } => write!(
                f,
                "authorized client action '{action_id}' references invalid function index {function_index}"
            ),
            Self::Vm(error) => write!(f, "client action VM error: {error}"),
            Self::NonStringResult { action_id } => write!(
                f,
                "client action '{action_id}' returned a non-string value; expected canonical nulang-ui-msg/1 JSON"
            ),
            Self::InvalidUtf8Result { action_id } => write!(
                f,
                "client action '{action_id}' returned non-UTF-8 result bytes"
            ),
            Self::Output(error) => write!(f, "invalid client action output: {error}"),
            Self::OutputDocumentMismatch { expected, found } => write!(
                f,
                "client action output document mismatch: expected '{expected}', got '{found}'"
            ),
            Self::PatchBaseRevisionMismatch { expected, found } => write!(
                f,
                "client action patch base revision mismatch: expected {expected}, got {found}"
            ),
        }
    }
}

impl std::error::Error for ClientActionRuntimeError {}

impl From<MobileNbcError> for ClientActionRuntimeError {
    fn from(value: MobileNbcError) -> Self {
        Self::Artifact(value)
    }
}

impl ClientActionRuntime {
    /// Construct an interpreter action runtime from mobile-aware `.nbc` bytes.
    /// Plain `.nbc` files are accepted but authorize no client actions.
    pub fn from_nbc(bytes: &[u8]) -> Result<Self, ClientActionRuntimeError> {
        let MobileNbcArtifact {
            artifact,
            mobile_actions,
        } = MobileNbcArtifact::from_nbc(bytes)?;
        Ok(Self {
            module: artifact.module,
            actions: mobile_actions,
        })
    }

    /// Construct directly from an already-decoded artifact. Primarily useful
    /// to package/runtime layers that have already verified artifact integrity.
    pub fn from_artifact(artifact: MobileNbcArtifact) -> Self {
        Self {
            module: artifact.artifact.module,
            actions: artifact.mobile_actions,
        }
    }

    pub fn authorized_action_ids(&self) -> impl Iterator<Item = &str> {
        self.actions
            .client_actions
            .iter()
            .map(|entry| entry.action_id.as_str())
    }

    /// Parse, authorize, execute, and validate one canonical native action.
    pub fn invoke_json(
        &self,
        request_json: &str,
    ) -> Result<RuntimeToHostMessage, ClientActionRuntimeError> {
        let request = decode_client_action_invocation(request_json)
            .map_err(ClientActionRuntimeError::Request)?;
        self.invoke(&request)
    }

    /// Execute one already-parsed canonical action request.
    ///
    /// `request.action_id` is the only host-visible authorization identity.
    /// The source handler name retained in the artifact is audit metadata; the
    /// allowlisted function-table index is the execution authority.
    pub fn invoke(
        &self,
        request: &ActionRequest,
    ) -> Result<RuntimeToHostMessage, ClientActionRuntimeError> {
        let request_json = encode_client_action_invocation(request)
            .map_err(ClientActionRuntimeError::Request)?;
        let action_id = request.action_id.as_str();

        let action = self
            .actions
            .client_actions
            .iter()
            .find(|entry| entry.action_id == action_id)
            .ok_or_else(|| ClientActionRuntimeError::UnauthorizedAction {
                action_id: action_id.to_owned(),
            })?;

        let function_index = action.function_index as usize;
        let code_offset = *self
            .module
            .function_table
            .get(function_index)
            .ok_or_else(|| ClientActionRuntimeError::InvalidFunctionIndex {
                action_id: action.action_id.clone(),
                function_index: action.function_index,
            })?;

        // Clone per invocation so the request string is interned in exactly the
        // module whose function consumes it. This avoids ambient/global string
        // provenance and keeps independent runtimes isolated.
        let mut module = self.module.clone();
        let request_string_id = module.add_string_constant(request_json) as u32;

        let mut vm = VM::new_without_jit();
        vm.load_module(module);
        let value = vm
            .call_function(0, code_offset, &[Value::string(request_string_id)])
            .map_err(|error| ClientActionRuntimeError::Vm(error.to_string()))?;

        let result_bytes = vm.string_bytes(value).ok_or_else(|| {
            ClientActionRuntimeError::NonStringResult {
                action_id: action.action_id.clone(),
            }
        })?;
        let result_json = String::from_utf8(result_bytes).map_err(|_| {
            ClientActionRuntimeError::InvalidUtf8Result {
                action_id: action.action_id.clone(),
            }
        })?;

        let output = decode_client_action_output(&result_json)
            .map_err(ClientActionRuntimeError::Output)?;
        validate_output_causality(request, &output)?;
        Ok(output)
    }
}

fn validate_output_causality(
    request: &ActionRequest,
    output: &RuntimeToHostMessage,
) -> Result<(), ClientActionRuntimeError> {
    match output {
        RuntimeToHostMessage::Snapshot { document, .. } => {
            if document.document_id != request.document_id {
                return Err(ClientActionRuntimeError::OutputDocumentMismatch {
                    expected: request.document_id.as_str().to_owned(),
                    found: document.document_id.as_str().to_owned(),
                });
            }
        }
        RuntimeToHostMessage::Patch { patch, .. } => {
            if patch.document_id != request.document_id {
                return Err(ClientActionRuntimeError::OutputDocumentMismatch {
                    expected: request.document_id.as_str().to_owned(),
                    found: patch.document_id.as_str().to_owned(),
                });
            }
            if patch.base_revision != request.revision {
                return Err(ClientActionRuntimeError::PatchBaseRevisionMismatch {
                    expected: request.revision.0,
                    found: patch.base_revision.0,
                });
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::mobile_nbc::{ClientActionEntry, MobileActionMetadata, CLIENT_ACTION_ABI};
    use crate::lexer::Lexer;
    use crate::mobile::action::stable_client_action_id;
    use crate::parser::Parser;
    use crate::typechecker::TypeChecker;
    use nulang_ui_protocol::{
        ActionId, ActionPlacement, CorrelationId, DocumentId, IdempotencyKey, Revision,
        WireValue,
    };

    fn compile_module(source: &str) -> crate::bytecode::CodeModule {
        let tokens = Lexer::new(source).lex().expect("lex");
        let ast = Parser::new(tokens).parse_module().expect("parse");
        let mut types = TypeChecker::new();
        types.check_module(&ast).expect("typecheck");
        let hir = crate::hir_lower::lower_module(&ast, &types.inferred_decl_types);
        let mut mir = crate::mir_lower::lower_module(&hir).expect("MIR lower");
        crate::mir_codegen::compile_mir(&mut mir, "mobile-action-runtime")
            .expect("bytecode compile")
    }

    fn runtime_for(source: &str, handler: &str) -> (ClientActionRuntime, ActionId) {
        let module = compile_module(source);
        let function_index = module
            .function_index_by_name(handler)
            .expect("compiled handler") as u32;
        let action_id =
            stable_client_action_id(&module.name, handler).expect("opaque action identity");
        let metadata = MobileActionMetadata {
            abi: CLIENT_ACTION_ABI.to_owned(),
            client_actions: vec![ClientActionEntry {
                action_id: action_id.as_str().to_owned(),
                handler: handler.to_owned(),
                function_index,
            }],
        };
        let bytes = module
            .to_mobile_nbc(None, &metadata)
            .expect("mobile nbc encode");
        (
            ClientActionRuntime::from_nbc(&bytes).expect("mobile action runtime"),
            action_id,
        )
    }

    fn request(action_id: ActionId, document_id: &str, revision: u64) -> ActionRequest {
        ActionRequest {
            document_id: DocumentId::new(document_id),
            revision: Revision(revision),
            action_id,
            placement: ActionPlacement::Client,
            correlation_id: CorrelationId::new("corr-1"),
            idempotency_key: IdempotencyKey::new("idem-1"),
            payload: WireValue::Null,
        }
    }

    #[test]
    fn authorized_action_executes_and_validates_canonical_patch() {
        let (runtime, action_id) = runtime_for(
            r#"
fn save(request: String) -> String {
    "{\"type\":\"patch\",\"protocol\":\"nulang-ui-msg/1\",\"patch\":{\"protocol\":\"nulang-ui/1\",\"document_id\":\"doc-1\",\"base_revision\":\"7\",\"revision\":\"8\",\"operations\":[]}}"
}
"#,
            "save",
        );
        let output = runtime
            .invoke(&request(action_id, "doc-1", 7))
            .expect("invoke authorized action");
        assert!(matches!(output, RuntimeToHostMessage::Patch { .. }));
    }

    #[test]
    fn unknown_action_is_rejected_before_execution() {
        let (runtime, _action_id) = runtime_for(
            r#"
fn save(request: String) -> String {
    "{\"type\":\"patch\",\"protocol\":\"nulang-ui-msg/1\",\"patch\":{\"protocol\":\"nulang-ui/1\",\"document_id\":\"doc-1\",\"base_revision\":\"7\",\"revision\":\"8\",\"operations\":[]}}"
}
"#,
            "save",
        );
        let request = request(ActionId::new("action_unknown"), "doc-1", 7);
        assert!(matches!(
            runtime.invoke(&request),
            Err(ClientActionRuntimeError::UnauthorizedAction { ref action_id })
                if action_id == "action_unknown"
        ));
    }

    #[test]
    fn output_cannot_switch_documents() {
        let (runtime, action_id) = runtime_for(
            r#"
fn save(request: String) -> String {
    "{\"type\":\"patch\",\"protocol\":\"nulang-ui-msg/1\",\"patch\":{\"protocol\":\"nulang-ui/1\",\"document_id\":\"other\",\"base_revision\":\"7\",\"revision\":\"8\",\"operations\":[]}}"
}
"#,
            "save",
        );
        assert!(matches!(
            runtime.invoke(&request(action_id, "doc-1", 7)),
            Err(ClientActionRuntimeError::OutputDocumentMismatch { .. })
        ));
    }

    #[test]
    fn patch_must_continue_from_invocation_revision() {
        let (runtime, action_id) = runtime_for(
            r#"
fn save(request: String) -> String {
    "{\"type\":\"patch\",\"protocol\":\"nulang-ui-msg/1\",\"patch\":{\"protocol\":\"nulang-ui/1\",\"document_id\":\"doc-1\",\"base_revision\":\"6\",\"revision\":\"8\",\"operations\":[]}}"
}
"#,
            "save",
        );
        assert!(matches!(
            runtime.invoke(&request(action_id, "doc-1", 7)),
            Err(ClientActionRuntimeError::PatchBaseRevisionMismatch {
                expected: 7,
                found: 6
            })
        ));
    }

    #[test]
    fn plain_nbc_authorizes_nothing() {
        let module = compile_module(
            r#"
fn save(request: String) -> String { request }
"#,
        );
        let bytes = module.to_nbc(None).expect("plain nbc");
        let runtime = ClientActionRuntime::from_nbc(&bytes).expect("runtime");
        let request = request(ActionId::new("action_unknown"), "doc-1", 7);
        assert!(matches!(
            runtime.invoke(&request),
            Err(ClientActionRuntimeError::UnauthorizedAction { .. })
        ));
    }
}
