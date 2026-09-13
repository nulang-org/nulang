//! Interpreter-only execution for compiler-authorized native client actions.
//!
//! This layer consumes the versioned action wire protocol and the allowlist
//! frozen into a mobile-aware `.nbc` artifact. It deliberately has no C/Swift
//! surface: native bindings should wrap this primitive only after its Rust
//! authorization, module-local string handling, and result validation are
//! proven independently.

use std::fmt;

use crate::format::mobile_nbc::{MobileActionMetadata, MobileNbcArtifact, MobileNbcError};
use crate::mobile::action::{
    ClientActionProtocolError, ClientActionRequest, ClientActionResult,
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
    Result(ClientActionProtocolError),
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
                "client action '{action_id}' returned a non-string value; expected nulang-action-result/1 JSON"
            ),
            Self::InvalidUtf8Result { action_id } => write!(
                f,
                "client action '{action_id}' returned non-UTF-8 result bytes"
            ),
            Self::Result(error) => write!(f, "invalid client action result: {error}"),
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

    /// Parse, authorize, execute, and validate one native client action.
    pub fn invoke_json(&self, request_json: &str) -> Result<ClientActionResult, ClientActionRuntimeError> {
        let request = ClientActionRequest::from_json(request_json)
            .map_err(ClientActionRuntimeError::Request)?;
        self.invoke(&request)
    }

    /// Execute one already-parsed request.
    ///
    /// `request.handler` is interpreted as the host-visible action identity.
    /// The internal handler name in the artifact is audit metadata only; the
    /// compiled function-table index is the actual execution authority.
    pub fn invoke(
        &self,
        request: &ClientActionRequest,
    ) -> Result<ClientActionResult, ClientActionRuntimeError> {
        request
            .validate()
            .map_err(ClientActionRuntimeError::Request)?;

        let action = self
            .actions
            .client_actions
            .iter()
            .find(|entry| entry.action_id == request.handler)
            .ok_or_else(|| ClientActionRuntimeError::UnauthorizedAction {
                action_id: request.handler.clone(),
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
        // module whose function will consume it. This avoids ambient/global
        // string provenance and keeps concurrent runtimes isolated.
        let mut module = self.module.clone();
        let request_json = request
            .to_json()
            .map_err(ClientActionRuntimeError::Request)?;
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

        ClientActionResult::from_json(&result_json, &request.correlation_id)
            .map_err(ClientActionRuntimeError::Result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::mobile_nbc::{ClientActionEntry, MobileActionMetadata, CLIENT_ACTION_ABI};
    use crate::lexer::Lexer;
    use crate::parser::Parser;
    use crate::typechecker::TypeChecker;

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

    fn runtime_for(source: &str, handler: &str) -> ClientActionRuntime {
        let module = compile_module(source);
        let function_index = module
            .function_offset_by_name(handler)
            .expect("compiled handler") as u32;
        let metadata = MobileActionMetadata {
            abi: CLIENT_ACTION_ABI.to_owned(),
            client_actions: vec![ClientActionEntry {
                action_id: handler.to_owned(),
                handler: handler.to_owned(),
                function_index,
            }],
        };
        let bytes = module
            .to_mobile_nbc(None, &metadata)
            .expect("mobile nbc encode");
        ClientActionRuntime::from_nbc(&bytes).expect("mobile action runtime")
    }

    #[test]
    fn authorized_action_executes_and_validates_result() {
        let runtime = runtime_for(
            r#"
fn save(request: String) -> String {
    "{\"protocol\":\"nulang-action-result/1\",\"correlation_id\":\"corr-1\",\"messages\":[]}"
}
"#,
            "save",
        );
        let request = ClientActionRequest::new("save", "corr-1", "save:corr-1");
        let result = runtime.invoke(&request).expect("invoke authorized action");
        assert_eq!(result.correlation_id, "corr-1");
        assert!(result.messages.is_empty());
    }

    #[test]
    fn unknown_action_is_rejected_before_execution() {
        let runtime = runtime_for(
            r#"
fn save(request: String) -> String {
    "{\"protocol\":\"nulang-action-result/1\",\"correlation_id\":\"corr-1\",\"messages\":[]}"
}
"#,
            "save",
        );
        let request = ClientActionRequest::new("secret", "corr-1", "secret:corr-1");
        assert!(matches!(
            runtime.invoke(&request),
            Err(ClientActionRuntimeError::UnauthorizedAction { ref action_id })
                if action_id == "secret"
        ));
    }

    #[test]
    fn result_cannot_switch_correlation_identity() {
        let runtime = runtime_for(
            r#"
fn save(request: String) -> String {
    "{\"protocol\":\"nulang-action-result/1\",\"correlation_id\":\"other\",\"messages\":[]}"
}
"#,
            "save",
        );
        let request = ClientActionRequest::new("save", "corr-1", "save:corr-1");
        assert!(matches!(
            runtime.invoke(&request),
            Err(ClientActionRuntimeError::Result(
                ClientActionProtocolError::CorrelationMismatch { .. }
            ))
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
        let request = ClientActionRequest::new("save", "corr-1", "save:corr-1");
        assert!(matches!(
            runtime.invoke(&request),
            Err(ClientActionRuntimeError::UnauthorizedAction { .. })
        ));
    }
}
