//! Compile-time web route contracts.
//!
//! This module bridges Nulang's existing `perform Web.route(...)` surface to a
//! structured contract representation. The deployment IR can consume these
//! contracts without relying on string scans for handler metadata.
//!
//! The first version is deliberately backwards compatible with `:name` path
//! parameters. It also understands `{name}` and `{name: Type}` segments so the
//! IR format is ready for typed route syntax without forcing a runtime migration
//! in the same change.

use crate::ast::{
    AstModule, Decl, Expr, FunctionAnnotation, Literal, Param, WebRequestParamSource, WorkflowItem,
};
use crate::lexer::Lexer;
use crate::parser::Parser;
use crate::types::{EffectRow, Type};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteParamContract {
    pub name: String,
    /// Source-level type name when known. Legacy `:name` segments inherit the
    /// type from a same-named handler parameter when one is declared.
    pub ty: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestParamSource {
    Path,
    Query,
    Header,
    Cookie,
    Body,
    Form,
    Json,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestParamBindingContract {
    pub source: RequestParamSource,
    pub source_name: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandlerParamContract {
    pub name: String,
    pub ty: Option<String>,
    pub capability: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request: Option<RequestParamBindingContract>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteContract {
    pub method: String,
    pub path: String,
    pub handler: Option<String>,
    pub params: Vec<RouteParamContract>,
    pub handler_params: Vec<HandlerParamContract>,
    pub response_type: Option<String>,
    pub error_type: Option<String>,
    pub effects: Vec<String>,
    /// Nulang reference capability (`iso`, `ref`, `val`, ...), when the handler
    /// declares one. Resource/security capabilities will be represented by a
    /// separate field once capability-parameterized effects land.
    pub reference_capability: Option<String>,
    pub placement: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContractCompilation {
    pub routes: Vec<RouteContract>,
    /// Best-effort source discovery must not make deployment IR generation
    /// panic. Parse and contract-validation failures are retained here so build
    /// tooling can surface them and can become hard errors once the contract
    /// pass is integrated into the typed compiler pipeline.
    pub diagnostics: Vec<String>,
}

#[derive(Clone)]
struct FunctionMeta {
    params: Vec<Param>,
    /// `using` parameters: appended to the compiled function's parameter list
    /// by HIR lowering, so the VM function's arity exceeds `params.len()`.
    /// Request bindings can never supply them.
    using_params: Vec<Param>,
    /// Typeclass dictionary parameters appended by HIR lowering (one per
    /// constraint class). Like `using_params`, they extend the VM arity
    /// beyond what a route binding plan can stage.
    dict_param_count: usize,
    request_bindings: HashMap<String, RequestParamBindingContract>,
    response_type: Option<String>,
    error_type: Option<String>,
    effects: Vec<String>,
    reference_capability: Option<String>,
    placement: Option<String>,
}

/// Preserve the compiler's existing Option variant representation in Web
/// contracts. The frontend expands Option[T] to the structural variant
/// Some(T) | None; the wire contract needs an unambiguous nullable shape.
/// Other variants retain their original names and fail JSON validation.
fn web_wire_type(ty: &Type) -> String {
    match ty {
        Type::Record(fields) => format!(
            "{{ {} }}",
            fields
                .iter()
                .map(|(name, field)| format!("{name}: {}", web_wire_type(field)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Type::Variant(variants) if variants.len() == 2 => {
            let payload = variants.iter().find_map(
                |(name, ty)| {
                    if name == "Some" {
                        ty.as_ref()
                    } else {
                        None
                    }
                },
            );
            let has_none = variants
                .iter()
                .any(|(name, ty)| name == "None" && ty.is_none());
            if let (Some(inner), true) = (payload, has_none) {
                format!("Option[{}]", web_wire_type(inner))
            } else {
                ty.to_string()
            }
        }
        _ => ty.to_string(),
    }
}

impl FunctionMeta {
    fn from_decl(decl: &Decl) -> Option<(String, Self)> {
        let Decl::Function {
            name,
            params,
            type_param_constraints,
            using_params,
            ret_type,
            error_type,
            effect,
            cap,
            annotations,
            ..
        } = decl
        else {
            return None;
        };

        let dict_param_count = type_param_constraints
            .iter()
            .map(|(_, _, classes)| classes.len())
            .sum();

        let placement = annotations.iter().find_map(|annotation| match annotation {
            FunctionAnnotation::Placement(p) => Some(p.to_string()),
            _ => None,
        });
        let request_bindings = annotations
            .iter()
            .filter_map(|annotation| match annotation {
                FunctionAnnotation::RequestBinding {
                    param,
                    source,
                    source_name,
                } => Some((
                    param.clone(),
                    RequestParamBindingContract {
                        source: request_source_contract(*source),
                        source_name: source_name.clone(),
                    },
                )),
                _ => None,
            })
            .collect();

        Some((
            name.clone(),
            Self {
                params: params.clone(),
                using_params: using_params.clone(),
                dict_param_count,
                request_bindings,
                response_type: ret_type.as_ref().map(web_wire_type),
                error_type: error_type.as_ref().map(ToString::to_string),
                effects: effect_names(effect.as_ref()),
                reference_capability: cap.map(|c| c.to_string()),
                placement,
            },
        ))
    }
}

/// Compile contracts from every `.nula` source file below `src_root`.
///
/// Parsing is intentionally file-local and best effort. Package builds have
/// already validated their source through the normal compiler pipeline; this
/// pass exists to preserve static web metadata in the deployment artifact.
pub fn compile_contracts_from_tree(src_root: &Path) -> ContractCompilation {
    let mut out = ContractCompilation::default();
    collect_contracts_recursive(src_root, &mut out);
    out.routes
        .sort_by(|a, b| (&a.method, &a.path).cmp(&(&b.method, &b.path)));
    out.routes
        .dedup_by(|a, b| a.method == b.method && a.path == b.path);
    out
}

fn collect_contracts_recursive(path: &Path, out: &mut ContractCompilation) {
    if path.is_dir() {
        let Ok(entries) = std::fs::read_dir(path) else {
            return;
        };
        let mut entries: Vec<_> = entries.filter_map(Result::ok).collect();
        entries.sort_by_key(|e| e.path());
        for entry in entries {
            collect_contracts_recursive(&entry.path(), out);
        }
        return;
    }

    if path.extension().and_then(|e| e.to_str()) != Some("nula") {
        return;
    }

    let source = match std::fs::read_to_string(path) {
        Ok(source) => source,
        Err(err) => {
            out.diagnostics
                .push(format!("{}: failed to read source: {err}", path.display()));
            return;
        }
    };

    let tokens = match Lexer::new(&source).lex() {
        Ok(tokens) => tokens,
        Err(err) => {
            out.diagnostics
                .push(format!("{}: lexer error: {err}", path.display()));
            return;
        }
    };
    let module = match Parser::new(tokens).parse_module() {
        Ok(module) => module,
        Err(err) => {
            out.diagnostics
                .push(format!("{}: parser error: {err}", path.display()));
            return;
        }
    };

    let compiled = compile_module_contracts(&module);
    out.routes.extend(compiled.routes);
    out.diagnostics.extend(
        compiled
            .diagnostics
            .into_iter()
            .map(|diagnostic| format!("{}: {diagnostic}", path.display())),
    );
}

/// Compile and validate static route contracts from a parsed module.
pub fn compile_module_contracts(module: &AstModule) -> ContractCompilation {
    let functions: HashMap<String, FunctionMeta> = module
        .decls
        .iter()
        .filter_map(FunctionMeta::from_decl)
        .collect();
    let transparent_aliases: HashMap<String, String> = module
        .decls
        .iter()
        .filter_map(|decl| match decl {
            Decl::TypeAlias {
                name,
                body,
                opaque: false,
                ..
            } => Some((name.clone(), body.to_string())),
            _ => None,
        })
        .collect();

    let named_json_records: HashMap<String, String> = module
        .decls
        .iter()
        .filter_map(|decl| {
            if let Decl::RecordType {
                name,
                type_params,
                fields,
                ..
            } = decl
            {
                if type_params.is_empty() {
                    return Some((
                        name.clone(),
                        format!(
                            "{{ {} }}",
                            fields
                                .iter()
                                .map(|(name, ty)| format!("{name}: {}", web_wire_type(ty)))
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                    ));
                }
            }
            None
        })
        .collect();

    let mut raw_routes = Vec::new();
    let mut route_diagnostics = Vec::new();
    collect_routes_in_module(module, &mut raw_routes, &mut route_diagnostics);

    let mut out = ContractCompilation::default();
    out.diagnostics.extend(route_diagnostics);
    for (method, path, handler, json_response) in raw_routes {
        let handler_name = match &handler {
            Expr::Var(name, _) => Some(name.clone()),
            _ => None,
        };
        let meta = handler_name
            .as_ref()
            .and_then(|name| functions.get(name.as_str()));

        let mut params = match parse_path_params(&path) {
            Ok(params) => params,
            Err(message) => {
                out.diagnostics.push(format!("{method} {path}: {message}"));
                Vec::new()
            }
        };

        if let Some(meta) = meta {
            if !meta.using_params.is_empty() || meta.dict_param_count > 0 {
                // HIR lowering appends `using` and typeclass-dictionary
                // parameters to the compiled function, so the VM arity exceeds
                // the request-bound parameter list. A binding plan built from
                // `params` alone would stage the closure into a parameter slot
                // (ClosureCall copies the whole register bank). Reject the
                // route instead of emitting a wrong-arity plan.
                let extra = [
                    (!meta.using_params.is_empty())
                        .then(|| format!("{} using", meta.using_params.len())),
                    (meta.dict_param_count > 0)
                        .then(|| format!("{} typeclass dictionary", meta.dict_param_count)),
                ]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" and ");
                out.diagnostics.push(format!(
                    "{method} {path}: handler '{}' has {extra} parameters that request bindings cannot supply",
                    handler_name.as_deref().unwrap_or("<handler>"),
                ));
                continue;
            }
            validate_and_infer_param_types(
                &method,
                &path,
                handler_name.as_deref().unwrap_or("<handler>"),
                &mut params,
                &meta.params,
                &meta.request_bindings,
                &transparent_aliases,
                &mut out.diagnostics,
            );
        }

        out.routes.push(RouteContract {
            method: method.clone(),
            path: path.clone(),
            handler: handler_name,
            params,
            handler_params: meta
                .map(|m| {
                    m.params
                        .iter()
                        .map(|param| {
                            let mut contract =
                                handler_param_contract(param, m.request_bindings.get(&param.name));
                            if contract
                                .request
                                .as_ref()
                                .is_some_and(|request| request.source == RequestParamSource::Json)
                            {
                                if let Some(named) = contract
                                    .ty
                                    .as_ref()
                                    .and_then(|ty| named_json_records.get(ty))
                                {
                                    contract.ty = Some(named.clone());
                                }
                            }
                            contract
                        })
                        .collect()
                })
                .unwrap_or_default(),
            // Route registration owns the wire media type even when an
            // opaque Json return type is erased to its String representation
            // by frontend type normalization.
            response_type: if json_response == "route_json_typed" {
                let source_ty = meta.and_then(|m| m.response_type.as_deref());
                let ty = source_ty.and_then(|ty| named_json_records.get(ty).map(String::as_str).or(Some(ty)));
                match ty {
                    Some(ty) if matches!(ty, "String" | "Int" | "Float" | "Bool")
                        || crate::web::json_records::parse_json_type(ty).is_some() =>
                    {
                        Some(format!("Json[{ty}]"))
                    }
                    _ => {
                        out.diagnostics.push(format!(
                            "{method} {path}: typed JSON responses require a declared String, Int, Float, Bool, or flat primitive record return type"
                        ));
                        None
                    }
                }
            } else if json_response == "route_json" {
                Some("Json".to_string())
            } else {
                meta.and_then(|m| m.response_type.clone())
            },
            error_type: meta.and_then(|m| m.error_type.clone()),
            effects: meta.map(|m| m.effects.clone()).unwrap_or_default(),
            reference_capability: meta.and_then(|m| m.reference_capability.clone()),
            placement: meta.and_then(|m| m.placement.clone()),
        });
    }

    out
}

/// Backwards-compatible convenience API for callers that only need routes.
pub fn contracts_from_module(module: &AstModule) -> Vec<RouteContract> {
    compile_module_contracts(module).routes
}

fn validate_and_infer_param_types(
    method: &str,
    path: &str,
    handler_name: &str,
    route_params: &mut [RouteParamContract],
    handler_params: &[Param],
    request_bindings: &HashMap<String, RequestParamBindingContract>,
    transparent_aliases: &HashMap<String, String>,
    diagnostics: &mut Vec<String>,
) {
    for route_param in route_params {
        let handler_param =
            handler_params
                .iter()
                .find(|param| match request_bindings.get(&param.name) {
                    Some(binding) => {
                        binding.source == RequestParamSource::Path
                            && binding.source_name == route_param.name
                    }
                    None => param.name == route_param.name,
                });
        let handler_ty = handler_param
            .and_then(|param| param.ty.as_ref())
            .map(ToString::to_string);

        match (&route_param.ty, handler_ty) {
            (Some(route_ty), Some(handler_ty))
                if canonical_type_name(route_ty, transparent_aliases) != handler_ty =>
            {
                diagnostics.push(format!(
                    "{method} {path}: route parameter '{}' is typed as {} but handler '{}' declares {}",
                    route_param.name, route_ty, handler_name, handler_ty
                ));
            }
            (Some(route_ty), None) => {
                diagnostics.push(format!(
                    "{method} {path}: typed route parameter '{}: {}' has no same-named typed parameter on handler '{}'",
                    route_param.name, route_ty, handler_name
                ));
            }
            (None, Some(handler_ty)) => {
                route_param.ty = Some(handler_ty);
            }
            _ => {}
        }
    }
}

fn canonical_type_name(name: &str, aliases: &HashMap<String, String>) -> String {
    let mut current = name.to_string();
    for _ in 0..=aliases.len() {
        let Some(next) = aliases.get(&current) else {
            break;
        };
        if next == &current {
            break;
        }
        current = next.clone();
    }
    current
}

fn request_source_contract(source: WebRequestParamSource) -> RequestParamSource {
    match source {
        WebRequestParamSource::Path => RequestParamSource::Path,
        WebRequestParamSource::Query => RequestParamSource::Query,
        WebRequestParamSource::Header => RequestParamSource::Header,
        WebRequestParamSource::Cookie => RequestParamSource::Cookie,
        WebRequestParamSource::Body => RequestParamSource::Body,
        WebRequestParamSource::Form => RequestParamSource::Form,
        WebRequestParamSource::Json => RequestParamSource::Json,
    }
}

fn handler_param_contract(
    param: &Param,
    request: Option<&RequestParamBindingContract>,
) -> HandlerParamContract {
    HandlerParamContract {
        name: param.name.clone(),
        ty: param.ty.as_ref().map(web_wire_type),
        capability: param.cap.map(|cap| cap.to_string()),
        request: request.cloned(),
    }
}

fn effect_names(row: Option<&EffectRow>) -> Vec<String> {
    let effects = match row {
        Some(EffectRow::Closed(effects)) | Some(EffectRow::Open(effects, _)) => effects,
        None => return Vec::new(),
    };
    effects.iter().map(ToString::to_string).collect()
}

/// Parse route parameters from legacy and contract-first path syntax.
///
/// Supported forms:
/// - `/users/:id`
/// - `/users/{id}`
/// - `/users/{id: UserId}`
pub fn parse_path_params(path: &str) -> Result<Vec<RouteParamContract>, String> {
    let mut params = Vec::new();
    for segment in path.split('/') {
        if let Some(name) = segment.strip_prefix(':') {
            if name.is_empty() {
                return Err(format!("route '{path}' contains an empty parameter"));
            }
            params.push(RouteParamContract {
                name: name.to_string(),
                ty: None,
            });
            continue;
        }

        if segment.starts_with('{') || segment.ends_with('}') {
            if !(segment.starts_with('{') && segment.ends_with('}')) {
                return Err(format!(
                    "route '{path}' contains malformed parameter '{segment}'"
                ));
            }
            let inner = &segment[1..segment.len() - 1];
            let (name, ty) = match inner.split_once(':') {
                Some((name, ty)) => (name.trim(), Some(ty.trim())),
                None => (inner.trim(), None),
            };
            if name.is_empty() {
                return Err(format!("route '{path}' contains an empty parameter"));
            }
            if matches!(ty, Some("")) {
                return Err(format!(
                    "route '{path}' parameter '{name}' has an empty type"
                ));
            }
            params.push(RouteParamContract {
                name: name.to_string(),
                ty: ty.map(str::to_string),
            });
        }
    }
    Ok(params)
}

fn collect_routes_in_module(
    module: &AstModule,
    out: &mut Vec<(String, String, Expr, String)>,
    diagnostics: &mut Vec<String>,
) {
    collect_routes_in_decls(&module.decls, out, diagnostics);
}

fn collect_routes_in_decls(
    decls: &[Decl],
    out: &mut Vec<(String, String, Expr, String)>,
    diagnostics: &mut Vec<String>,
) {
    for decl in decls {
        match decl {
            Decl::Function { body, .. }
            | Decl::LetBinding { value: body, .. }
            | Decl::Signal { init: body, .. } => collect_routes_in_expr(body, out, diagnostics),
            Decl::Workflow { items, .. } => {
                for item in items {
                    match item {
                        WorkflowItem::Step(step) => {
                            collect_routes_in_expr(&step.body, out, diagnostics)
                        }
                        WorkflowItem::Parallel(steps) => {
                            for step in steps {
                                collect_routes_in_expr(&step.body, out, diagnostics);
                            }
                        }
                    }
                }
            }
            Decl::Module { decls, .. } => collect_routes_in_decls(decls, out, diagnostics),
            _ => {}
        }
    }
}

fn collect_routes_in_expr(
    expr: &Expr,
    out: &mut Vec<(String, String, Expr, String)>,
    diagnostics: &mut Vec<String>,
) {
    if let Expr::Perform {
        effect, op, args, ..
    } = expr
    {
        if effect == "Web" {
            if matches!(op.as_str(), "route" | "route_json" | "route_json_typed") && args.len() == 3
            {
                if let (Some(method), Some(path)) =
                    (string_literal(&args[0]), string_literal(&args[1]))
                {
                    out.push((method, path, args[2].clone(), op.clone()));
                }
            } else if matches!(
                op.as_str(),
                "route_scoped" | "route_json_scoped" | "route_json_typed_scoped"
            ) {
                if let [method, prefix, path, handler] = args.as_slice() {
                    match (
                        string_literal(method),
                        string_literal(prefix),
                        string_literal(path),
                    ) {
                        (Some(method), Some(prefix), Some(path)) => {
                            match crate::web::route_prefix::join_scoped_route_path(&prefix, &path) {
                                Ok(path) => {
                                    let normalized_op = op
                                        .strip_suffix("_scoped")
                                        .unwrap_or(op.as_str())
                                        .to_string();
                                    out.push((method, path, handler.clone(), normalized_op));
                                }
                                Err(why) => diagnostics.push(format!(
                                    "invalid scoped route {method} {prefix}{path}: {why}"
                                )),
                            }
                        }
                        _ => diagnostics.push(format!(
                            "Web.{op} requires literal method, prefix, and route path"
                        )),
                    }
                } else {
                    diagnostics.push(format!("Web.{op} expects four arguments"));
                }
            }
        }
        for arg in args {
            collect_routes_in_expr(arg, out, diagnostics);
        }
        return;
    }

    match expr {
        Expr::Lambda { body, .. } => collect_routes_in_expr(body, out, diagnostics),
        Expr::App { func, args, .. } => {
            collect_routes_in_expr(func, out, diagnostics);
            for arg in args {
                collect_routes_in_expr(arg, out, diagnostics);
            }
        }
        Expr::Let { value, body, .. } | Expr::LetRec { value, body, .. } => {
            collect_routes_in_expr(value, out, diagnostics);
            collect_routes_in_expr(body, out, diagnostics);
        }
        Expr::If {
            cond,
            then_branch,
            else_branch,
            ..
        } => {
            collect_routes_in_expr(cond, out, diagnostics);
            collect_routes_in_expr(then_branch, out, diagnostics);
            if let Some(else_branch) = else_branch {
                collect_routes_in_expr(else_branch, out, diagnostics);
            }
        }
        Expr::Match {
            scrutinee, arms, ..
        } => {
            collect_routes_in_expr(scrutinee, out, diagnostics);
            for (_, guard, body) in arms {
                if let Some(guard) = guard {
                    collect_routes_in_expr(guard, out, diagnostics);
                }
                collect_routes_in_expr(body, out, diagnostics);
            }
        }
        Expr::Block { exprs, .. } | Expr::Par { exprs, .. } => {
            for expr in exprs {
                collect_routes_in_expr(expr, out, diagnostics);
            }
        }
        Expr::Tuple(exprs, ..) | Expr::Array(exprs, ..) | Expr::FString(exprs, ..) => {
            for expr in exprs {
                collect_routes_in_expr(expr, out, diagnostics);
            }
        }
        Expr::Record(fields, ..) | Expr::RecordUpdate { fields, .. } => {
            for (_, expr) in fields {
                collect_routes_in_expr(expr, out, diagnostics);
            }
        }
        Expr::FieldAccess { expr, .. }
        | Expr::Unary { expr, .. }
        | Expr::Consume { expr, .. }
        | Expr::CapAnnotate { expr, .. }
        | Expr::TypeAnnotate { expr, .. } => collect_routes_in_expr(expr, out, diagnostics),
        Expr::Index { arr, idx, .. } => {
            collect_routes_in_expr(arr, out, diagnostics);
            collect_routes_in_expr(idx, out, diagnostics);
        }
        Expr::Binary { left, right, .. } | Expr::Pipe { left, right, .. } => {
            collect_routes_in_expr(left, out, diagnostics);
            collect_routes_in_expr(right, out, diagnostics);
        }
        Expr::Assign { target, value, .. } => {
            collect_routes_in_expr(target, out, diagnostics);
            collect_routes_in_expr(value, out, diagnostics);
        }
        Expr::For { iterable, body, .. } => {
            collect_routes_in_expr(iterable, out, diagnostics);
            collect_routes_in_expr(body, out, diagnostics);
        }
        Expr::While { cond, body, .. } => {
            collect_routes_in_expr(cond, out, diagnostics);
            collect_routes_in_expr(body, out, diagnostics);
        }
        Expr::Return(value, ..) | Expr::Break(value, ..) => {
            if let Some(value) = value {
                collect_routes_in_expr(value, out, diagnostics);
            }
        }
        Expr::Recover { body, .. } | Expr::Hide { body, .. } | Expr::Seal { body, .. } => {
            collect_routes_in_expr(body, out, diagnostics)
        }
        Expr::Defer { expr, .. } => collect_routes_in_expr(expr, out, diagnostics),
        Expr::Handle { body, handlers, .. } => {
            collect_routes_in_expr(body, out, diagnostics);
            for handler in handlers {
                collect_routes_in_expr(&handler.body, out, diagnostics);
            }
        }
        Expr::Emit { args, .. } => {
            for arg in args {
                collect_routes_in_expr(arg, out, diagnostics);
            }
        }
        Expr::Spawn {
            actor_type,
            init,
            positional_args,
            target_node,
            ..
        } => {
            collect_routes_in_expr(actor_type, out, diagnostics);
            for (_, expr) in init {
                collect_routes_in_expr(expr, out, diagnostics);
            }
            if let Some(args) = positional_args {
                for arg in args {
                    collect_routes_in_expr(arg, out, diagnostics);
                }
            }
            if let Some(target) = target_node {
                collect_routes_in_expr(target, out, diagnostics);
            }
        }
        Expr::Send { actor, args, .. } | Expr::Ask { actor, args, .. } => {
            collect_routes_in_expr(actor, out, diagnostics);
            for arg in args {
                collect_routes_in_expr(arg, out, diagnostics);
            }
        }
        Expr::Receive { arms, after, .. } => {
            for (_, _, guard, body) in arms {
                if let Some(guard) = guard {
                    collect_routes_in_expr(guard, out, diagnostics);
                }
                collect_routes_in_expr(body, out, diagnostics);
            }
            if let Some((timeout, body)) = after {
                collect_routes_in_expr(timeout, out, diagnostics);
                collect_routes_in_expr(body, out, diagnostics);
            }
        }
        Expr::GrainRef { key, .. } => collect_routes_in_expr(key, out, diagnostics),
        Expr::Resume { value, .. } => collect_routes_in_expr(value, out, diagnostics),
        Expr::Migrate { actor, node, .. } => {
            collect_routes_in_expr(actor, out, diagnostics);
            collect_routes_in_expr(node, out, diagnostics);
        }
        _ => {}
    }
}

fn string_literal(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Literal(Literal::String(value), ..) => Some(value.clone()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(source: &str) -> AstModule {
        let tokens = Lexer::new(source).lex().unwrap();
        Parser::new(tokens).parse_module().unwrap()
    }

    #[test]
    fn parses_legacy_and_typed_path_params() {
        let legacy = parse_path_params("/users/:id/posts/:post_id").unwrap();
        assert_eq!(legacy[0].name, "id");
        assert_eq!(legacy[0].ty, None);

        let typed = parse_path_params("/orgs/{org_id: OrgId}/members/{member_id}").unwrap();
        assert_eq!(typed[0].name, "org_id");
        assert_eq!(typed[0].ty.as_deref(), Some("OrgId"));
        assert_eq!(typed[1].name, "member_id");
        assert_eq!(typed[1].ty, None);
    }

    #[test]
    fn scoped_routes_compose_literal_prefix_before_contract_validation() {
        let ast = parse(
            r#"
fn show_user(id: String from path) -> String { id }
fn show_me() -> String { "me" }
fn register() {
    perform Web.route_scoped("GET", "/api/v1", "/users/{id: String}", show_user)
    perform Web.route_json_typed_scoped("GET", "/api/v1", "/users/me", show_me)
}
"#,
        );
        let contracts = compile_module_contracts(&ast);
        assert_eq!(contracts.routes.len(), 2, "{:?}", contracts.diagnostics);
        assert!(
            contracts.diagnostics.is_empty(),
            "{:?}",
            contracts.diagnostics
        );
        assert_eq!(contracts.routes[0].path, "/api/v1/users/{id: String}");
        assert_eq!(contracts.routes[0].params[0].name, "id");
        assert_eq!(contracts.routes[1].path, "/api/v1/users/me");
        assert_eq!(
            contracts.routes[1].response_type.as_deref(),
            Some("Json[String]")
        );
    }

    #[test]
    fn scoped_route_rejects_ambiguous_or_traversal_prefixes() {
        let ast = parse(
            r#"
fn show() -> String { "OK" }
fn register() {
    perform Web.route_scoped("GET", "/api/../admin", "/users", show)
    perform Web.route_scoped("GET", "/api", "//other", show)
    perform Web.route_scoped("GET", "/api%2fprivate", "/users", show)
}
"#,
        );
        let contracts = compile_module_contracts(&ast);
        assert_eq!(contracts.routes.len(), 0, "{:?}", contracts.routes);
        assert_eq!(
            contracts.diagnostics.len(),
            3,
            "{:?}",
            contracts.diagnostics
        );
    }

    #[test]
    fn typed_json_registration_rejects_unsupported_return_shapes() {
        let module = parse(
            r#"
fn show() -> [Int] { [1, 2] }
fn register() { perform Web.route_json_typed("GET", "/array", show) }
"#,
        );
        let compiled = compile_module_contracts(&module);
        assert!(
            compiled
                .diagnostics
                .iter()
                .any(|message| message.contains("typed JSON responses require a declared")),
            "{:?}",
            compiled.diagnostics
        );
        assert_eq!(compiled.routes.len(), 1);
        assert!(compiled.routes[0].response_type.is_none());
    }

    #[test]
    fn contract_inherits_legacy_param_type_from_handler() {
        let module = parse(
            r#"
type UserId = String

fn show_user(id: UserId) -> String ! {DB, Request} {
    let _ = perform Web.param("id")
    "ok"
}

fn web_main() {
    perform Web.route("GET", "/users/:id", show_user)
}
"#,
        );

        let compiled = compile_module_contracts(&module);
        assert!(
            compiled.diagnostics.is_empty(),
            "{:?}",
            compiled.diagnostics
        );
        assert_eq!(compiled.routes.len(), 1);
        let route = &compiled.routes[0];
        assert_eq!(route.handler.as_deref(), Some("show_user"));
        assert_eq!(route.params[0].ty.as_deref(), Some("String"));
        assert_eq!(route.response_type.as_deref(), Some("String"));
        assert!(route.effects.contains(&"DB".to_string()));
        assert!(route.effects.contains(&"Request".to_string()));
    }

    #[test]
    fn typed_path_matches_handler_parameter_type() {
        let module = parse(
            r#"
type UserId = String

fn show_user(id: UserId) -> String {
    "ok"
}

fn web_main() {
    perform Web.route("GET", "/users/{id: UserId}", show_user)
}
"#,
        );

        let compiled = compile_module_contracts(&module);
        assert!(
            compiled.diagnostics.is_empty(),
            "{:?}",
            compiled.diagnostics
        );
        assert_eq!(compiled.routes[0].params[0].ty.as_deref(), Some("UserId"));
    }

    #[test]
    fn typed_path_mismatch_is_diagnostic() {
        let module = parse(
            r#"
type UserId = String
type ExternalId = Int

fn show_user(id: UserId) -> String {
    "ok"
}

fn web_main() {
    perform Web.route("GET", "/users/{id: ExternalId}", show_user)
}
"#,
        );

        let compiled = compile_module_contracts(&module);
        assert_eq!(compiled.diagnostics.len(), 1);
        assert!(compiled.diagnostics[0].contains("typed as ExternalId"));
        assert!(compiled.diagnostics[0].contains("declares String"));
    }

    #[test]
    fn using_params_on_route_handler_is_diagnostic() {
        // HIR lowering appends using/typeclass-dictionary parameters to the
        // compiled function; a binding plan built from `params` alone would
        // stage the closure into a parameter slot.
        let module = parse(
            r#"
fn show(id: Int) using (log: Int) -> String {
    "ok"
}

fn web_main() {
    perform Web.route("GET", "/users/{id: Int}", show)
}
"#,
        );

        let compiled = compile_module_contracts(&module);
        assert_eq!(compiled.diagnostics.len(), 1);
        assert!(compiled.diagnostics[0].contains("using"));
        assert!(compiled.routes.is_empty());
    }

    #[test]
    fn typeclass_constraints_on_route_handler_is_diagnostic() {
        let module = parse(
            r#"
fn show[T: Show](x: T) -> String {
    "ok"
}

fn web_main() {
    perform Web.route("GET", "/users/{x: Int}", show)
}
"#,
        );

        let compiled = compile_module_contracts(&module);
        assert_eq!(compiled.diagnostics.len(), 1);
        assert!(compiled.diagnostics[0].contains("typeclass dictionary"));
        assert!(compiled.routes.is_empty());
    }
}
