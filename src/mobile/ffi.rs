//! C ABI for the compiler-authorized native client-action runtime.
//!
//! This is intentionally separate from the generic `NulangRuntime` embedding
//! surface. Native action execution has a narrower authority model: only
//! actions frozen into the mobile `.nbc` allowlist can run.

use std::ffi::{c_char, CStr, CString};

use crate::mobile::action::encode_client_action_output;
use crate::mobile::runtime::ClientActionRuntime;

/// Opaque C-owned wrapper around [`ClientActionRuntime`].
///
/// Construction always returns an object so callers can retrieve a detailed
/// artifact/metadata error with `nulang_mobile_action_runtime_last_error`.
/// `is_ready` reports whether the artifact decoded successfully.
#[repr(C)]
pub struct NulangMobileActionRuntime {
    runtime: Option<ClientActionRuntime>,
    last_result: Option<CString>,
    last_error: Option<CString>,
}

impl NulangMobileActionRuntime {
    fn from_bytes(bytes: &[u8]) -> Self {
        match ClientActionRuntime::from_nbc(bytes) {
            Ok(runtime) => Self {
                runtime: Some(runtime),
                last_result: None,
                last_error: None,
            },
            Err(error) => Self {
                runtime: None,
                last_result: None,
                last_error: Some(safe_cstring(error.to_string())),
            },
        }
    }

    fn set_error(&mut self, message: impl Into<String>) {
        self.last_result = None;
        self.last_error = Some(safe_cstring(message.into()));
    }

    fn clear_transient(&mut self) {
        self.last_result = None;
        self.last_error = None;
    }
}

fn safe_cstring(value: String) -> CString {
    // JSON/error text can theoretically contain U+0000. C strings cannot, so
    // preserve the diagnostic as a printable escape rather than truncating it.
    let sanitized = value.replace('\0', "\\0");
    CString::new(sanitized).expect("NUL bytes were escaped before CString construction")
}

/// Create an action runtime from mobile-aware `.nbc` bytes.
///
/// The returned object must be released with
/// `nulang_mobile_action_runtime_free`. Invalid artifacts still return a
/// non-null object; query `is_ready` and `last_error` for the failure.
///
/// # Safety
/// When `len > 0`, `bytes` must point to at least `len` readable bytes for the
/// duration of this call. A null pointer is accepted only when `len == 0`.
#[no_mangle]
pub unsafe extern "C" fn nulang_mobile_action_runtime_new(
    bytes: *const u8,
    len: usize,
) -> *mut NulangMobileActionRuntime {
    let input: &[u8] = if len == 0 {
        &[]
    } else if bytes.is_null() {
        // Preserve the always-return-an-object contract for constructor
        // failures so the host can inspect a stable error string.
        return Box::into_raw(Box::new(NulangMobileActionRuntime {
            runtime: None,
            last_result: None,
            last_error: Some(safe_cstring(
                "mobile action artifact pointer is null while len is non-zero".to_string(),
            )),
        }));
    } else {
        std::slice::from_raw_parts(bytes, len)
    };

    Box::into_raw(Box::new(NulangMobileActionRuntime::from_bytes(input)))
}

/// Return whether construction decoded a valid mobile action artifact.
///
/// # Safety
/// `runtime` must be null or a live pointer returned by
/// `nulang_mobile_action_runtime_new`.
#[no_mangle]
pub unsafe extern "C" fn nulang_mobile_action_runtime_is_ready(
    runtime: *const NulangMobileActionRuntime,
) -> bool {
    runtime
        .as_ref()
        .map(|runtime| runtime.runtime.is_some())
        .unwrap_or(false)
}

/// Invoke one canonical `nulang-ui-msg/1` client action request.
///
/// Returns a NUL-terminated canonical `nulang-ui-msg/1` snapshot/patch JSON
/// string owned by the runtime, or null on failure. The result pointer remains
/// valid until the next invocation on this runtime or until the runtime is
/// freed.
///
/// # Safety
/// `runtime` must be a live pointer returned by the constructor. `request_json`
/// must point to a readable NUL-terminated UTF-8 string for the duration of the
/// call. Calls on one runtime instance must be externally serialized.
#[no_mangle]
pub unsafe extern "C" fn nulang_mobile_action_runtime_invoke(
    runtime: *mut NulangMobileActionRuntime,
    request_json: *const c_char,
) -> *const c_char {
    let Some(runtime) = runtime.as_mut() else {
        return std::ptr::null();
    };
    runtime.clear_transient();

    if request_json.is_null() {
        runtime.set_error("client action request pointer is null");
        return std::ptr::null();
    }

    let request = match CStr::from_ptr(request_json).to_str() {
        Ok(request) => request,
        Err(error) => {
            runtime.set_error(format!("client action request is not UTF-8: {error}"));
            return std::ptr::null();
        }
    };

    let Some(action_runtime) = runtime.runtime.as_ref() else {
        runtime.set_error("mobile action runtime is not ready");
        return std::ptr::null();
    };

    let result = match action_runtime.invoke_json(request) {
        Ok(result) => result,
        Err(error) => {
            runtime.set_error(error.to_string());
            return std::ptr::null();
        }
    };

    let result_json = match encode_client_action_output(&result) {
        Ok(result_json) => result_json,
        Err(error) => {
            runtime.set_error(format!("failed to encode client action output: {error}"));
            return std::ptr::null();
        }
    };
    runtime.last_result = Some(safe_cstring(result_json));
    runtime
        .last_result
        .as_ref()
        .map(|result| result.as_ptr())
        .unwrap_or(std::ptr::null())
}

/// Return the most recent action-runtime error, or null when none is present.
/// The pointer is owned by the runtime and remains valid until the next
/// invocation or until the runtime is freed.
///
/// # Safety
/// `runtime` must be null or a live pointer returned by the constructor.
#[no_mangle]
pub unsafe extern "C" fn nulang_mobile_action_runtime_last_error(
    runtime: *const NulangMobileActionRuntime,
) -> *const c_char {
    runtime
        .as_ref()
        .and_then(|runtime| runtime.last_error.as_ref())
        .map(|error| error.as_ptr())
        .unwrap_or(std::ptr::null())
}

/// Free a mobile action runtime. Null is accepted as a no-op.
///
/// # Safety
/// A non-null pointer must have been returned by
/// `nulang_mobile_action_runtime_new` and must not already have been freed.
#[no_mangle]
pub unsafe extern "C" fn nulang_mobile_action_runtime_free(
    runtime: *mut NulangMobileActionRuntime,
) {
    if !runtime.is_null() {
        drop(Box::from_raw(runtime));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::mobile_nbc::{ClientActionEntry, MobileActionMetadata, CLIENT_ACTION_ABI};
    use crate::lexer::Lexer;
    use crate::mobile::action::{encode_client_action_invocation, stable_client_action_id};
    use crate::parser::Parser;
    use crate::typechecker::TypeChecker;
    use nulang_ui_protocol::{
        ActionId, ActionPlacement, ActionRequest, CorrelationId, DocumentId, IdempotencyKey,
        Revision, WireValue,
    };

    fn artifact() -> (Vec<u8>, ActionId) {
        let source = r#"
fn save(request: String) -> String {
    "{\"type\":\"patch\",\"protocol\":\"nulang-ui-msg/1\",\"patch\":{\"protocol\":\"nulang-ui/1\",\"document_id\":\"doc-ffi\",\"base_revision\":\"7\",\"revision\":\"8\",\"operations\":[]}}"
}
"#;
        let tokens = Lexer::new(source).lex().expect("lex");
        let ast = Parser::new(tokens).parse_module().expect("parse");
        let mut types = TypeChecker::new();
        types.check_module(&ast).expect("typecheck");
        let hir = crate::hir_lower::lower_module(&ast, &types.inferred_decl_types);
        let mut mir = crate::mir_lower::lower_module(&hir).expect("MIR lower");
        let module = crate::mir_codegen::compile_mir(&mut mir, "mobile-action-ffi")
            .expect("bytecode compile");
        let function_index = module.function_index_by_name("save").expect("save") as u32;
        let action_id =
            stable_client_action_id(&module.name, "save").expect("opaque action identity");
        let metadata = MobileActionMetadata {
            abi: CLIENT_ACTION_ABI.to_owned(),
            client_actions: vec![ClientActionEntry {
                action_id: action_id.as_str().to_owned(),
                handler: "save".to_owned(),
                function_index,
            }],
        };
        (
            module.to_mobile_nbc(None, &metadata).expect("mobile nbc"),
            action_id,
        )
    }

    fn request(action_id: ActionId) -> CString {
        let request = ActionRequest {
            document_id: DocumentId::new("doc-ffi"),
            revision: Revision(7),
            action_id,
            placement: ActionPlacement::Client,
            correlation_id: CorrelationId::new("corr-ffi"),
            idempotency_key: IdempotencyKey::new("idem-ffi"),
            payload: WireValue::Null,
        };
        CString::new(encode_client_action_invocation(&request).expect("encode request"))
            .expect("request JSON has no NUL")
    }

    #[test]
    fn c_boundary_invokes_only_authorized_action() {
        let (bytes, action_id) = artifact();
        let runtime = unsafe { nulang_mobile_action_runtime_new(bytes.as_ptr(), bytes.len()) };
        assert!(!runtime.is_null());
        assert!(unsafe { nulang_mobile_action_runtime_is_ready(runtime) });

        let request = request(action_id);
        let result = unsafe { nulang_mobile_action_runtime_invoke(runtime, request.as_ptr()) };
        assert!(!result.is_null());
        let result = unsafe { CStr::from_ptr(result) }.to_str().unwrap();
        let json: serde_json::Value = serde_json::from_str(result).unwrap();
        assert_eq!(json["protocol"], "nulang-ui-msg/1");
        assert_eq!(json["type"], "patch");
        assert_eq!(json["patch"]["document_id"], "doc-ffi");
        assert_eq!(json["patch"]["base_revision"], "7");

        let unauthorized = request(ActionId::new("action_unknown"));
        let result = unsafe { nulang_mobile_action_runtime_invoke(runtime, unauthorized.as_ptr()) };
        assert!(result.is_null());
        let error = unsafe { nulang_mobile_action_runtime_last_error(runtime) };
        assert!(!error.is_null());
        assert!(unsafe { CStr::from_ptr(error) }
            .to_str()
            .unwrap()
            .contains("not authorized"));

        unsafe { nulang_mobile_action_runtime_free(runtime) };
    }

    #[test]
    fn invalid_artifact_returns_queryable_not_ready_runtime() {
        let bytes = b"not-an-nbc";
        let runtime = unsafe { nulang_mobile_action_runtime_new(bytes.as_ptr(), bytes.len()) };
        assert!(!runtime.is_null());
        assert!(!unsafe { nulang_mobile_action_runtime_is_ready(runtime) });
        let error = unsafe { nulang_mobile_action_runtime_last_error(runtime) };
        assert!(!error.is_null());
        assert!(!unsafe { CStr::from_ptr(error) }.to_bytes().is_empty());
        unsafe { nulang_mobile_action_runtime_free(runtime) };
    }

    #[test]
    fn null_nonzero_artifact_pointer_fails_without_ub() {
        let runtime = unsafe { nulang_mobile_action_runtime_new(std::ptr::null(), 1) };
        assert!(!runtime.is_null());
        assert!(!unsafe { nulang_mobile_action_runtime_is_ready(runtime) });
        let error = unsafe { nulang_mobile_action_runtime_last_error(runtime) };
        assert!(!error.is_null());
        unsafe { nulang_mobile_action_runtime_free(runtime) };
    }
}
