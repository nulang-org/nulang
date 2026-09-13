//! Runtime execution support for compiler-produced web route bindings.
//!
//! The compiler owns route syntax and produces [`RouteBindingContract`] values.
//! This module is deliberately transport-agnostic: it accepts already-captured
//! request values and stages them into the VM call ABI (`r0..rN`, `argc`) without
//! re-parsing route contracts or consulting ambient request state.

use crate::bytecode::{CodeModule, Constant, Instruction, OpCode};
use crate::runtime::WebRoute;
use crate::vm::VM;
use crate::web::bindings::{compile_route_bindings, RouteBindingContract, RouteBindingSource};
use crate::web::contracts::{ContractCompilation, RouteContract};
use std::collections::HashMap;

/// One precompiled route-pattern segment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeRouteSegment {
    Literal(String),
    PathParam(String),
}

/// Runtime projection of a compiler route contract.
///
/// Pattern syntax is compiled once here. Request dispatch therefore performs
/// only segment comparison/capture; it does not need to rediscover handler
/// argument order or source types from the route string on every request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeRoutePlan {
    pub method: String,
    pub path: String,
    pub segments: Vec<RuntimeRouteSegment>,
    pub bindings: Vec<RouteBindingContract>,
    pub handler_param_count: usize,
    /// True when every declared handler parameter is supplied directly from a
    /// route input and every path parameter has a direct binding. Legacy routes
    /// that still rely on ambient `Web.param` access keep this false.
    pub direct_call: bool,
}

/// A low-level runtime registration paired with optional compiler metadata.
///
/// Keeping this as a sidecar avoids teaching the `Web.route` host effect about
/// source-level types. Registration remains method/path/function; package build
/// and dev tooling attach the richer contract once source analysis is complete.
#[derive(Clone, Debug)]
pub struct RuntimeWebRoute {
    pub route: WebRoute,
    pub plan: Option<RuntimeRoutePlan>,
}

#[derive(Clone, Debug, Default)]
pub struct RuntimeRouteAttachment {
    pub routes: Vec<RuntimeWebRoute>,
    pub diagnostics: Vec<String>,
}

/// Attach package-level compiler contracts to routes collected by the VM.
///
/// Missing contracts are preserved as legacy runtime registrations. Invalid
/// compiler contracts keep the route available for diagnostics but do not opt
/// it into contract-first dispatch.
pub fn attach_runtime_route_plans(
    routes: Vec<WebRoute>,
    compilation: &ContractCompilation,
) -> RuntimeRouteAttachment {
    let contract_index: HashMap<(&str, &str), &RouteContract> = compilation
        .routes
        .iter()
        .map(|contract| ((contract.method.as_str(), contract.path.as_str()), contract))
        .collect();

    let mut diagnostics = compilation.diagnostics.clone();
    let routes = routes
        .into_iter()
        .map(|route| {
            let key = (route.method.as_str(), route.path.as_str());
            let plan =
                contract_index.get(&key).and_then(|contract| {
                    match compile_runtime_route_plan(contract) {
                        Ok(plan) => Some(plan),
                        Err(mut route_diagnostics) => {
                            diagnostics.append(&mut route_diagnostics);
                            None
                        }
                    }
                });
            RuntimeWebRoute { route, plan }
        })
        .collect();

    diagnostics.sort();
    diagnostics.dedup();
    RuntimeRouteAttachment {
        routes,
        diagnostics,
    }
}

/// Compile a source-level route contract into a runtime matching/call plan.
pub fn compile_runtime_route_plan(
    contract: &RouteContract,
) -> Result<RuntimeRoutePlan, Vec<String>> {
    let binding_compilation = compile_route_bindings(contract);
    if !binding_compilation.diagnostics.is_empty() {
        return Err(binding_compilation.diagnostics);
    }

    let segments = compile_route_segments(&contract.path).map_err(|diagnostic| {
        vec![format!(
            "{} {}: {diagnostic}",
            contract.method, contract.path
        )]
    })?;
    let direct_call = binding_compilation.bindings.len() == contract.handler_params.len()
        && binding_compilation.bindings.len() == contract.params.len();

    Ok(RuntimeRoutePlan {
        method: contract.method.clone(),
        path: contract.path.clone(),
        segments,
        bindings: binding_compilation.bindings,
        handler_param_count: contract.handler_params.len(),
        direct_call,
    })
}

/// Match a request path using a precompiled route plan.
pub fn match_runtime_route(
    plan: &RuntimeRoutePlan,
    request_path: &str,
) -> Option<HashMap<String, String>> {
    match_segments(&plan.segments, request_path)
}

/// Match a collected route, preferring its compiler plan and falling back to
/// the existing `:name` runtime convention when no contract was attached.
pub fn match_attached_route(
    route: &RuntimeWebRoute,
    request_path: &str,
) -> Option<HashMap<String, String>> {
    match &route.plan {
        Some(plan) => match_runtime_route(plan, request_path),
        None => compile_legacy_route_segments(&route.route.path)
            .ok()
            .and_then(|segments| match_segments(&segments, request_path)),
    }
}

fn match_segments(
    segments: &[RuntimeRouteSegment],
    request_path: &str,
) -> Option<HashMap<String, String>> {
    let request_path = request_path.trim_start_matches('/');
    let request_segments: Vec<&str> = if request_path.is_empty() {
        vec![""]
    } else {
        request_path.split('/').collect()
    };
    if segments.len() != request_segments.len() {
        return None;
    }

    let mut params = HashMap::new();
    for (pattern, value) in segments.iter().zip(request_segments) {
        match pattern {
            RuntimeRouteSegment::Literal(expected) if expected != value => return None,
            RuntimeRouteSegment::Literal(_) => {}
            RuntimeRouteSegment::PathParam(name) => {
                params.insert(name.clone(), value.to_string());
            }
        }
    }
    Some(params)
}

fn compile_route_segments(path: &str) -> Result<Vec<RuntimeRouteSegment>, String> {
    compile_segments(path, true)
}

fn compile_legacy_route_segments(path: &str) -> Result<Vec<RuntimeRouteSegment>, String> {
    compile_segments(path, false)
}

fn compile_segments(
    path: &str,
    allow_contract_params: bool,
) -> Result<Vec<RuntimeRouteSegment>, String> {
    let normalized = path.trim_start_matches('/');
    let parts: Vec<&str> = if normalized.is_empty() {
        vec![""]
    } else {
        normalized.split('/').collect()
    };

    parts
        .into_iter()
        .map(|segment| {
            if let Some(name) = segment.strip_prefix(':') {
                if name.is_empty() {
                    return Err("route contains an empty legacy path parameter".to_string());
                }
                return Ok(RuntimeRouteSegment::PathParam(name.to_string()));
            }

            if allow_contract_params && (segment.starts_with('{') || segment.ends_with('}')) {
                let Some(inner) = segment
                    .strip_prefix('{')
                    .and_then(|segment| segment.strip_suffix('}'))
                else {
                    return Err(format!("malformed path parameter '{segment}'"));
                };
                let name = inner.split_once(':').map_or(inner, |(name, _)| name).trim();
                if name.is_empty() {
                    return Err("route contains an empty contract path parameter".to_string());
                }
                return Ok(RuntimeRouteSegment::PathParam(name.to_string()));
            }

            Ok(RuntimeRouteSegment::Literal(segment.to_string()))
        })
        .collect()
}

/// One VM argument produced from a compiler binding.
#[derive(Debug, Clone, PartialEq)]
pub struct BoundRouteArgument {
    pub handler_index: usize,
    pub value: Constant,
}

/// Convert captured path values into a complete handler argument vector.
///
/// Every handler slot must have exactly one compiler-produced binding. This is
/// intentionally strict: silently filling an unbound slot with `nil` would turn
/// a compiler contract violation into request-time behavior.
pub fn bind_path_arguments(
    bindings: &[RouteBindingContract],
    handler_param_count: usize,
    path_params: &HashMap<String, String>,
) -> Result<Vec<BoundRouteArgument>, String> {
    if handler_param_count > u8::MAX as usize {
        return Err(format!(
            "route handler has {handler_param_count} parameters; VM call ABI supports at most {}",
            u8::MAX
        ));
    }

    let mut slots: Vec<Option<BoundRouteArgument>> = vec![None; handler_param_count];
    for binding in bindings {
        if binding.source != RouteBindingSource::Path {
            return Err(format!(
                "unsupported route binding source for handler parameter '{}'",
                binding.handler_param
            ));
        }
        if binding.handler_index >= handler_param_count {
            return Err(format!(
                "binding for '{}' targets handler slot {} but handler has {} parameters",
                binding.handler_param, binding.handler_index, handler_param_count
            ));
        }
        if slots[binding.handler_index].is_some() {
            return Err(format!(
                "multiple route bindings target handler slot {}",
                binding.handler_index
            ));
        }

        let raw = path_params.get(&binding.source_name).ok_or_else(|| {
            format!(
                "missing captured path parameter '{}' for handler parameter '{}'",
                binding.source_name, binding.handler_param
            )
        })?;
        let value = decode_path_constant(raw, binding.ty.as_deref()).map_err(|message| {
            format!(
                "path parameter '{}' for handler parameter '{}': {message}",
                binding.source_name, binding.handler_param
            )
        })?;
        slots[binding.handler_index] = Some(BoundRouteArgument {
            handler_index: binding.handler_index,
            value,
        });
    }

    slots
        .into_iter()
        .enumerate()
        .map(|(index, slot)| {
            slot.ok_or_else(|| format!("handler parameter slot {index} has no request binding"))
        })
        .collect()
}

/// Execute a handler with compiler-produced path bindings.
///
/// Arguments are materialized as constants into `r0..rN`, a non-capturing
/// closure for the handler is placed immediately after the argument bank, and
/// `ClosureCall` receives the real argument count. The returned VM value is
/// rendered with the module's normal string representation.
pub fn render_bound_route_handler(
    module: &CodeModule,
    func_idx: usize,
    bindings: &[RouteBindingContract],
    handler_param_count: usize,
    path_params: &HashMap<String, String>,
) -> Result<String, String> {
    let args = bind_path_arguments(bindings, handler_param_count, path_params)?;

    let mut vm = VM::new();
    let mut executable = module.clone();
    let entry_offset = executable.instructions.len();

    for arg in args {
        let constant_index = executable.add_constant(arg.value);
        if constant_index > u16::MAX as usize {
            return Err("route argument constant pool exceeds 16-bit ConstU index".to_string());
        }
        executable.emit(Instruction::new3(
            OpCode::ConstU,
            ((constant_index >> 8) & 0xFF) as u8,
            (constant_index & 0xFF) as u8,
            arg.handler_index as u8,
        ));
    }

    // `handler_param_count <= 255` above means r0..r(argc-1) hold arguments
    // and r(argc) remains available for the non-capturing closure, including
    // the maximum case where argc=255 and the closure occupies r255.
    let closure_reg = handler_param_count as u8;
    if func_idx > u16::MAX as usize {
        return Err(format!(
            "handler function index {func_idx} exceeds the 16-bit closure operand"
        ));
    }
    executable.emit(Instruction::new3(
        OpCode::Closure,
        ((func_idx >> 8) & 0xFF) as u8,
        (func_idx & 0xFF) as u8,
        closure_reg,
    ));
    executable.emit(Instruction::new3(
        OpCode::ClosureCall,
        closure_reg,
        handler_param_count as u8,
        0,
    ));
    executable.emit(Instruction::new0(OpCode::Ret));

    vm.load_module(executable);
    let result = vm
        .run_from(0, entry_offset)
        .map_err(|error| format!("route handler execution failed: {error}"))?;
    Ok(vm.value_to_string(0, result))
}

fn decode_path_constant(raw: &str, ty: Option<&str>) -> Result<Constant, String> {
    match ty.map(str::trim) {
        Some("Int") => raw
            .parse::<i64>()
            .map(Constant::Int)
            .map_err(|_| format!("expected Int, got '{raw}'")),
        Some("Float") => raw
            .parse::<f64>()
            .map(Constant::Float)
            .map_err(|_| format!("expected Float, got '{raw}'")),
        Some("Bool") => match raw {
            "true" => Ok(Constant::Bool(true)),
            "false" => Ok(Constant::Bool(false)),
            _ => Err(format!("expected Bool ('true' or 'false'), got '{raw}'")),
        },
        // `String`, untyped legacy parameters, and source-level aliases/opaque
        // identifier types are string-backed until the typed frontend exposes
        // their runtime representation in the Web Contract IR.
        _ => Ok(Constant::String(raw.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::contracts::{HandlerParamContract, RouteParamContract};

    fn binding(name: &str, index: usize, ty: Option<&str>) -> RouteBindingContract {
        RouteBindingContract {
            source: RouteBindingSource::Path,
            source_name: name.to_string(),
            handler_param: name.to_string(),
            handler_index: index,
            ty: ty.map(str::to_string),
        }
    }

    fn contract(path: &str) -> RouteContract {
        RouteContract {
            method: "GET".to_string(),
            path: path.to_string(),
            handler: Some("show_user".to_string()),
            params: vec![RouteParamContract {
                name: "id".to_string(),
                ty: Some("Int".to_string()),
            }],
            handler_params: vec![HandlerParamContract {
                name: "id".to_string(),
                ty: Some("Int".to_string()),
                capability: None,
            }],
            response_type: Some("String".to_string()),
            error_type: None,
            effects: Vec::new(),
            reference_capability: None,
            placement: Some("server".to_string()),
        }
    }

    #[test]
    fn compiles_and_matches_contract_route_once() {
        let plan = compile_runtime_route_plan(&contract("/users/{id: Int}")).unwrap();
        assert!(plan.direct_call);
        assert_eq!(
            plan.segments,
            vec![
                RuntimeRouteSegment::Literal("users".to_string()),
                RuntimeRouteSegment::PathParam("id".to_string())
            ]
        );
        let params = match_runtime_route(&plan, "/users/42").unwrap();
        assert_eq!(params.get("id"), Some(&"42".to_string()));
        assert!(match_runtime_route(&plan, "/accounts/42").is_none());
    }

    #[test]
    fn legacy_route_plan_keeps_colon_matching() {
        let plan = compile_runtime_route_plan(&contract("/users/:id")).unwrap();
        let params = match_runtime_route(&plan, "/users/9").unwrap();
        assert_eq!(params.get("id"), Some(&"9".to_string()));
    }

    #[test]
    fn legacy_ambient_route_is_not_marked_direct_call() {
        let mut ambient = contract("/users/:id");
        ambient.handler_params.clear();
        let plan = compile_runtime_route_plan(&ambient).unwrap();
        assert!(!plan.direct_call);
    }

    #[test]
    fn stages_arguments_by_handler_slot_not_path_order() {
        let bindings = vec![
            binding("org", 1, Some("String")),
            binding("user", 0, Some("Int")),
        ];
        let params = HashMap::from([
            ("org".to_string(), "acme".to_string()),
            ("user".to_string(), "42".to_string()),
        ]);

        let args = bind_path_arguments(&bindings, 2, &params).unwrap();
        assert_eq!(args[0].handler_index, 0);
        assert_eq!(args[0].value, Constant::Int(42));
        assert_eq!(args[1].handler_index, 1);
        assert_eq!(args[1].value, Constant::String("acme".to_string()));
    }

    #[test]
    fn rejects_missing_handler_slots() {
        let bindings = vec![binding("id", 1, Some("String"))];
        let params = HashMap::from([("id".to_string(), "abc".to_string())]);
        let error = bind_path_arguments(&bindings, 2, &params).unwrap_err();
        assert!(error.contains("slot 0"));
    }

    #[test]
    fn rejects_missing_captured_path_values() {
        let bindings = vec![binding("id", 0, Some("String"))];
        let error = bind_path_arguments(&bindings, 1, &HashMap::new()).unwrap_err();
        assert!(error.contains("missing captured path parameter 'id'"));
    }

    #[test]
    fn decodes_supported_primitive_types() {
        assert_eq!(
            decode_path_constant("7", Some("Int")).unwrap(),
            Constant::Int(7)
        );
        assert_eq!(
            decode_path_constant("3.5", Some("Float")).unwrap(),
            Constant::Float(3.5)
        );
        assert_eq!(
            decode_path_constant("true", Some("Bool")).unwrap(),
            Constant::Bool(true)
        );
    }

    #[test]
    fn keeps_custom_identifier_types_string_backed() {
        assert_eq!(
            decode_path_constant("usr_123", Some("UserId")).unwrap(),
            Constant::String("usr_123".to_string())
        );
    }

    #[test]
    fn reports_primitive_decode_failures() {
        assert!(decode_path_constant("not-an-int", Some("Int"))
            .unwrap_err()
            .contains("expected Int"));
        assert!(decode_path_constant("1", Some("Bool"))
            .unwrap_err()
            .contains("expected Bool"));
    }
}
