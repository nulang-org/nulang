//! Opt-in HTTP form admission: verify session-bound CSRF tokens, then validate
//! the exact compiler-derived form schema before invoking any route handler.
//!
//! The caller must authenticate the session independently, use a Secure
//! host-scoped session cookie, and keep the 32-byte signing key secret.
//! This module does not provide authentication or generate session identifiers.

use crate::web::bindings::RouteBindingSource;
use crate::web::forms::{FormContract, FormValidationErrors};
use crate::web::http_request::HttpRequestBindingInputs;
use crate::web::runtime_bindings::RuntimeWebRoute;

#[derive(Clone)]
pub struct CsrfPolicy {
    session_cookie_name: String,
    signing_key: [u8; 32],
}

impl std::fmt::Debug for CsrfPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CsrfPolicy")
            .field("session_cookie_name", &self.session_cookie_name)
            .field("signing_key", &"[redacted]")
            .finish()
    }
}

impl CsrfPolicy {
    /// Require a browser-enforced __Host- cookie (Secure, Path=/, no Domain
    /// when set by the host application). A zero key is not suitable for CSRF.
    pub fn new(session_cookie_name: &str, signing_key: [u8; 32]) -> Result<Self, String> {
        if !session_cookie_name.starts_with("__Host-")
            || session_cookie_name.len() <= "__Host-".len()
            || !session_cookie_name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        {
            return Err("CSRF requires a valid __Host- session cookie name".into());
        }
        if signing_key == [0; 32] {
            return Err("CSRF signing key must be a secret random 32-byte value".into());
        }
        Ok(Self {
            session_cookie_name: session_cookie_name.to_owned(),
            signing_key,
        })
    }

    /// Generate a token for an already authenticated session. Render it as a
    /// hidden input named __nulang_csrf in a separately escaped form wrapper.
    /// The token is unguessable without the configured server-side secret.
    pub fn token_for_session(&self, session: &str, method: &str, route_path: &str) -> String {
        let mut hasher = blake3::Hasher::new_keyed(&self.signing_key);
        hasher.update(b"nulang.web.csrf.v1");
        for part in [session, method, route_path] {
            hasher.update(&(part.len() as u64).to_be_bytes());
            hasher.update(part.as_bytes());
        }
        hasher.finalize().to_hex().to_string()
    }

    fn verify(
        &self,
        method: &str,
        headers: &[(String, String)],
        captured: &HttpRequestBindingInputs,
        route_path: &str,
    ) -> bool {
        // Cookie capture is intentionally *not* based on the legacy cookie map,
        // which silently overwrites duplicate names. Ambiguous cookie identity
        // is always denied, even across separate Cookie headers.
        let mut session = None;
        for (header, value) in headers {
            if !header.eq_ignore_ascii_case("cookie") {
                continue;
            }
            for part in value.split(';') {
                let Some((name, value)) = part.trim().split_once('=') else {
                    continue;
                };
                if name.trim() == self.session_cookie_name {
                    if session.is_some()
                        || value.is_empty()
                        || value
                            .bytes()
                            .any(|byte| !byte.is_ascii_graphic() || byte == b';')
                    {
                        return false;
                    }
                    session = Some(value);
                }
            }
        }
        let Some(session) = session else {
            return false;
        };

        let Some(values) = captured.form.get("__nulang_csrf") else {
            return false;
        };
        let [submitted] = values.as_slice() else {
            return false;
        };
        // Reject noncanonical or malformed encodings; do not normalize input.
        if submitted.len() != 64
            || !submitted
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return false;
        }
        let expected = self.token_for_session(session, method, route_path);
        // Compare all 64 bytes without an early return for an incorrect token.
        let mut difference = 0u8;
        for (actual, expected) in submitted.bytes().zip(expected.bytes()) {
            difference |= actual ^ expected;
        }
        std::hint::black_box(difference) == 0
    }
}

#[derive(Debug)]
pub enum FormGuardError {
    Forbidden,
    Validation(FormValidationErrors),
}

#[derive(Clone, Debug)]
pub struct GuardedFormRoute {
    pub form: FormContract,
    pub csrf: CsrfPolicy,
    // Compiler-owned field layout is kept with the same in-memory CodeModule.
    // Deserializing a module alone cannot turn on typed form dispatch.
    typed_module: Option<crate::mir_codegen::CompiledWebModule>,
}

impl GuardedFormRoute {
    pub fn new(form: FormContract, csrf: CsrfPolicy) -> Self {
        Self {
            form,
            csrf,
            typed_module: None,
        }
    }

    /// Explicit opt-in. The registered executable must be byte-for-byte the
    /// same module compiled with this authoritative record field layout.
    pub fn new_typed(
        form: FormContract,
        csrf: CsrfPolicy,
        compiled: crate::mir_codegen::CompiledWebModule,
    ) -> Self {
        Self {
            form,
            csrf,
            typed_module: Some(compiled),
        }
    }

    pub fn is_typed(&self) -> bool {
        self.typed_module.is_some()
    }

    /// Enter a typed VM handler after the ordinary CSRF+422 admission path.
    /// Only compiler-verified, form-only route plans can reach this method.
    pub fn render_typed(
        &self,
        route: &RuntimeWebRoute,
        captured: &HttpRequestBindingInputs,
    ) -> Result<String, String> {
        let compiled = self
            .typed_module
            .as_ref()
            .ok_or("missing typed compiler layout")?;
        let plan = route.plan.as_ref().ok_or("missing compiler route plan")?;
        let submitted = self
            .form
            .validate(&captured.form)
            .map_err(|_| "validated form no longer matches submitted request")?;
        let mut args = Vec::with_capacity(plan.bindings.len());
        for binding in &plan.bindings {
            let field = self
                .form
                .fields
                .iter()
                .find(|field| field.name == binding.source_name)
                .ok_or("typed form binding has no validated field")?;
            let value = match field.shape {
                crate::web::forms::FormFieldShape::Required => {
                    crate::web::handler_call::TypedWebValue::Scalar(
                        submitted
                            .values
                            .get(&field.name)
                            .ok_or("missing required validated value")?
                            .clone(),
                    )
                }
                crate::web::forms::FormFieldShape::Optional => {
                    crate::web::handler_call::TypedWebValue::OptionScalar(
                        submitted.values.get(&field.name).cloned(),
                    )
                }
                crate::web::forms::FormFieldShape::Repeated { .. } => {
                    crate::web::handler_call::TypedWebValue::ArrayScalars(
                        submitted
                            .repeated
                            .get(&field.name)
                            .ok_or("missing validated collection")?
                            .clone(),
                    )
                }
            };
            args.push(crate::web::handler_call::TypedBoundRouteArgument {
                handler_index: binding.handler_index,
                value,
            });
        }
        crate::web::handler_call::render_typed_bound_handler(
            compiled,
            route.route.handler_func_idx,
            plan.handler_param_count,
            &args,
        )
    }

    /// Validate a guard against exactly one live, compiler-bound route. No
    /// handler may run under an incomplete or mismatched guard configuration.
    pub fn check_route(&self, route: &RuntimeWebRoute) -> Result<(), String> {
        let Some(plan) = &route.plan else {
            return Err("guarded form route requires a compiler-owned plan".into());
        };
        if self.form.method != route.route.method.as_str()
            || self.form.path != route.route.path
            || self.form.method != plan.method
            || self.form.path != plan.path
        {
            return Err("form guard method/path does not match runtime and compiler route".into());
        }
        if !matches!(
            self.form.method.as_str(),
            "POST" | "PUT" | "PATCH" | "DELETE"
        ) {
            return Err("guarded form route must use a state-changing HTTP method".into());
        }
        if !crate::web::dispatch::has_complete_request_binding_plan(plan) {
            return Err("guarded form route requires a complete handler binding plan".into());
        }
        if let Some(compiled) = &self.typed_module {
            if compiled.module() != &route.route.handler_module {
                return Err(
                    "typed form layout and executable were not produced by the same compiler run"
                        .into(),
                );
            }
            if plan
                .bindings
                .iter()
                .any(|binding| binding.source != RouteBindingSource::Form)
            {
                return Err(
                    "typed form handler currently requires all arguments from form fields".into(),
                );
            }
        }
        let form_bindings: Vec<_> = plan
            .bindings
            .iter()
            .filter(|binding| binding.source == RouteBindingSource::Form)
            .collect();
        if form_bindings.is_empty() || form_bindings.len() != self.form.fields.len() {
            return Err("form guard fields do not match the compiler binding plan".into());
        }
        for (binding, field) in form_bindings.iter().zip(&self.form.fields) {
            if let Some(compiled) = &self.typed_module {
                let declaration = binding
                    .ty
                    .as_deref()
                    .ok_or("typed form requires an explicit source type")?;
                let compiler_type = compiled
                    .handler_param_type(route.route.handler_func_idx, binding.handler_index)
                    .ok_or("typed form binding is missing an authoritative MIR parameter type")?;
                if compiler_type.to_string() != declaration {
                    return Err(
                        "typed form contract type differs from compiled MIR handler signature"
                            .into(),
                    );
                }
                let expected_scalar = match field.shape {
                    crate::web::forms::FormFieldShape::Required => declaration,
                    crate::web::forms::FormFieldShape::Optional => {
                        let inner = declaration
                            .strip_prefix("Some ")
                            .and_then(|ty| ty.strip_suffix(" | None"))
                            .or_else(|| declaration.strip_prefix("Option[").and_then(|ty| ty.strip_suffix(']')))
                            .ok_or("optional form field requires compiler-resolved Some T | None or Option[T] handler type")?;
                        if compiled.record_field_id("ctor").is_none()
                            || compiled.record_field_id("payload").is_none()
                        {
                            return Err(
                                "compiler layout does not expose Option record fields".into()
                            );
                        }
                        inner
                    }
                    crate::web::forms::FormFieldShape::Repeated { .. } => declaration
                        .strip_prefix('[')
                        .and_then(|ty| ty.strip_suffix(']'))
                        .ok_or("repeated form field requires [T] handler annotation")?,
                };
                if !matches!(expected_scalar, "String" | "Int" | "Float" | "Bool") {
                    return Err("typed forms only support primitive scalar element types".into());
                }
            } else {
                // The legacy direct binder cannot construct variants or arrays.
                if field.shape != crate::web::forms::FormFieldShape::Required {
                    return Err(
                        "optional/repeated form inputs need typed VM binding support".into(),
                    );
                }
                if !crate::web::request_bindings::scalar_binding_supported(binding.ty.as_deref()) {
                    return Err(
                        "structured form handler type is not supported by scalar VM binding".into(),
                    );
                }
            }
            if field.name == "__nulang_csrf"
                || field.name == "__nulang_ui_message"
                || field.name == "__nulang_action"
                || field.name != binding.source_name
                || field.handler_param != binding.handler_param
                || field.source_type != binding.ty
            {
                return Err("form guard fields differ from the compiler binding plan or use reserved fields".into());
            }
        }
        self.form.check_configuration()
    }

    /// CSRF is checked before semantic constraints or the VM are evaluated.
    /// A failed guard is a 403, not an ordinary 422 validation error.
    pub fn preflight(
        &self,
        method: &str,
        headers: &[(String, String)],
        captured: &HttpRequestBindingInputs,
    ) -> Result<(), FormGuardError> {
        if !self.csrf.verify(method, headers, captured, &self.form.path) {
            return Err(FormGuardError::Forbidden);
        }
        self.form
            .validate(&captured.form)
            .map(|_| ())
            .map_err(FormGuardError::Validation)
    }
}
