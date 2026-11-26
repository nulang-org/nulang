//! Conservative build-time page rendering. Runtime routes remain in the
//! deployment contract but never produce an HTML artifact merely because
//! their URL has no path parameters.

use crate::bytecode::{CodeModule, Constant, OpCode};
use crate::runtime::WebRoute;
use crate::types::{NuError, Span};
use crate::vm::{Value, VM};
use crate::web::bindings::RouteBindingSource;
use crate::web::contracts::ContractCompilation;
use crate::web::dispatch::{has_complete_request_binding_plan, match_route};
use crate::web::handler_call::prepare_bound_handler;
use crate::web::ir::{DeploymentIr, IrRoute};
use crate::web::request_bindings::{bind_request_arguments, RequestBindingValues};
use crate::web::runtime_bindings::{
    BoundRouteArgument, RuntimeRoutePlan, RuntimeRouteSegment, RuntimeWebRoute,
};
use crate::web::static_proof::{check_instruction, prove_handler};
use std::collections::{BTreeMap, HashMap};

/// Discover registered routes without executing request, I/O, or mutation
/// effects in the application entry point. Route registration is the only
/// additional operation allowed compared to page rendering.
pub(crate) fn discover_routes(module: CodeModule) -> Result<Vec<WebRoute>, String> {
    let entry = module.entry_point.unwrap_or(0);
    let mut vm = VM::new_without_jit();
    vm.load_module(module);
    vm.run_from_checked(0, entry, |module, instr| {
        if instr.opcode == OpCode::Perform
            && matches!(module.constants.get(instr.imm16() as usize),
                Some(Constant::String(name)) if name == "Web.route")
        {
            return Ok(());
        }
        check_instruction(module, instr)
            .map_err(|reason| NuError::runtime_error(reason, Span::default()))
    })
    .map_err(|error| format!("web route registration failed: {error}"))?;
    Ok(vm.take_web_routes())
}

pub(crate) fn render_pages(
    routes: &[RuntimeWebRoute],
    contracts: &ContractCompilation,
    enumeration: &BTreeMap<String, Vec<String>>,
    collections: &BTreeMap<String, String>,
    ir: &mut DeploymentIr,
) -> Result<BTreeMap<String, String>, String> {
    if routes.len() != ir.routes.len() {
        return Err("static build route/IR length mismatch".into());
    }
    for pattern in enumeration.keys().chain(collections.keys()) {
        if enumeration.contains_key(pattern) && collections.contains_key(pattern) {
            return Err(format!(
                "prerender '{pattern}' is configured in both [web.prerender] and [web.collections]"
            ));
        }
        if routes
            .iter()
            .filter(|r| r.route.method.as_str() == "GET" && r.route.path == *pattern)
            .count()
            != 1
        {
            return Err(format!(
                "prerender '{pattern}' must identify exactly one registered GET route"
            ));
        }
        if contracts
            .routes
            .iter()
            .filter(|c| c.method == "GET" && c.path == *pattern)
            .count()
            != 1
        {
            return Err(format!(
                "prerender '{pattern}' must identify exactly one compiler route contract"
            ));
        }
        if enumeration.get(pattern).is_some_and(Vec::is_empty) {
            return Err(format!("prerender '{pattern}' has an empty URL list"));
        }
    }

    let mut literal_gets = BTreeMap::new();
    for metadata in &ir.routes {
        if metadata.method != "GET" {
            continue;
        }
        if let Ok(path) = artifact_path(&metadata.path) {
            if let Some(previous) =
                literal_gets.insert(path.to_ascii_lowercase(), metadata.path.clone())
            {
                return Err(format!(
                    "GET {} conflicts with GET {previous} at '{path}'",
                    metadata.path
                ));
            }
        }
    }
    let mut pages = BTreeMap::new();
    let mut paths = BTreeMap::<String, String>::new();
    let mut deployed = Vec::new();
    for (index, (route, mut metadata)) in routes
        .iter()
        .zip(std::mem::take(&mut ir.routes))
        .enumerate()
    {
        let label = format!("{} {}", metadata.method, metadata.path);
        let placement = contracts
            .routes
            .iter()
            .find(|contract| contract.method == metadata.method && contract.path == metadata.path)
            .and_then(|contract| contract.placement.as_deref());
        let explicitly_static = placement == Some("static");

        if metadata.method == "GET"
            && (enumeration.contains_key(&metadata.path)
                || collections.contains_key(&metadata.path))
        {
            if placement.is_some_and(|p| p != "static") {
                return Err(format!(
                    "{label}: prerender conflicts with explicit placement"
                ));
            }
            if let Err(reason) = prove_candidate(&metadata, &route.route, true) {
                return Err(format!("{label}: cannot prerender: {reason}"));
            }
            let mut urls = match collections.get(&metadata.path) {
                Some(provider) => collection_urls(route, provider)?,
                None => enumeration[&metadata.path].clone(),
            };
            urls.sort();
            for url in urls {
                let path = artifact_path(&url)?;
                // A concrete page must not shadow another GET handler, even
                // if the runtime registration order would choose this one.
                // Also reject portable filename aliases of literal routes.
                if let Some(other) = literal_gets.get(&path.to_ascii_lowercase()) {
                    return Err(format!(
                        "{label}: prerender '{url}' conflicts with GET {other}"
                    ));
                }
                let canonical = url.trim_end_matches('/');
                let slash_alias = format!("{canonical}/");
                if let Some(other) = routes.iter().enumerate().find(|(i, other)| {
                    *i != index
                        && other.route.method.as_str() == "GET"
                        && [url.as_str(), canonical, slash_alias.as_str()]
                            .iter()
                            .any(|path| match_route(other, path).is_some())
                }) {
                    return Err(format!(
                        "{label}: prerender '{url}' conflicts with GET {}",
                        other.1.route.path
                    ));
                }
                if url.starts_with("/__nulang/sse/") {
                    return Err(format!(
                        "{label}: prerender '{url}' conflicts with reserved SSE route"
                    ));
                }
                // Use exactly the compiler plan and scalar decoder used by
                // HTTP dispatch; never fabricate ambient request state.
                let args = prerender_arguments(route, &url)?;
                let page_label = format!("{label} -> {url}");
                reserve_path(&mut paths, &path, &page_label)?;
                let html = match render(&route.route, metadata.handler_params.len(), &args)? {
                    Rendered::Html(html) => html,
                    Rendered::Denied(reason) => {
                        return Err(format!("{page_label}: cannot prerender: {reason}"))
                    }
                };
                let mut concrete = metadata.clone();
                concrete.route_template = Some(metadata.path.clone());
                concrete.path = url;
                concrete.placement = "static".into();
                concrete.artifact = Some(path.clone());
                // The request inputs have been consumed at build time.
                // Concrete deployment routes do not have runtime slots.
                concrete.params.clear();
                concrete.handler_params.clear();
                concrete.bindings.clear();
                deployed.push(concrete);
                pages.insert(
                    path,
                    crate::web::reactivity::inject_client_runtime_script(&html),
                );
            }
            continue;
        }

        if metadata.placement == "static" {
            let html = match prove_candidate(&metadata, &route.route, false) {
                Err(reason) => Rendered::Denied(reason),
                Ok(()) => render(&route.route, 0, &[])?,
            };
            match html {
                Rendered::Html(html) => {
                    let path = artifact_path(&metadata.path)?;
                    reserve_path(&mut paths, &path, &label)?;
                    metadata.artifact = Some(path.clone());
                    pages.insert(
                        path,
                        crate::web::reactivity::inject_client_runtime_script(&html),
                    );
                }
                Rendered::Denied(reason) => {
                    if explicitly_static {
                        return Err(format!(
                            "{label}: cannot render @placement(static): {reason}"
                        ));
                    }
                    eprintln!("  {label}: retaining server placement: {reason}");
                    metadata.placement = "server".into();
                    metadata.artifact = None;
                }
            }
        }
        deployed.push(metadata);
    }
    ir.routes = deployed;
    Ok(pages)
}

fn prerender_plan<'a>(
    route: &'a RuntimeWebRoute,
    label: &str,
) -> Result<&'a RuntimeRoutePlan, String> {
    let plan = route
        .plan
        .as_ref()
        .ok_or_else(|| format!("{label}: unresolved route plan"))?;
    if !has_complete_request_binding_plan(plan)
        || plan
            .bindings
            .iter()
            .any(|b| b.source != RouteBindingSource::Path)
    {
        return Err(format!(
            "{label}: prerender requires complete path-only handler bindings"
        ));
    }
    // The scalar request decoder treats unknown types as strings for legacy
    // compatibility. Do not assume that an unresolved type has a string ABI.
    if plan
        .bindings
        .iter()
        .any(|b| !matches!(b.ty.as_deref(), Some("String" | "Int" | "Bool" | "Float")))
    {
        return Err(format!(
            "{label}: prerender requires explicitly typed String, Int, Bool or Float path bindings"
        ));
    }
    Ok(plan)
}

fn prerender_arguments(
    route: &RuntimeWebRoute,
    url: &str,
) -> Result<Vec<BoundRouteArgument>, String> {
    let label = format!("GET {} -> {url}", route.route.path);
    let plan = prerender_plan(route, &label)?;
    let path = match_route(route, url)
        .ok_or_else(|| format!("{label}: URL does not match route pattern"))?;
    if path.values().any(String::is_empty) {
        return Err(format!("{label}: empty path capture"));
    }
    let empty = HashMap::new();
    let values = RequestBindingValues {
        path: &path,
        query: &empty,
        headers: &[],
        cookies: &empty,
        body: None,
        form: &empty,
    };
    bind_request_arguments(&plan.bindings, plan.handler_param_count, &values)
        .map_err(|error| format!("{label}: {error}"))
}

/// Evaluate one native Nulang route-key projection in the same checked VM
/// as the renderer. Arrays contain scalars for a one-capture route, or tuples
/// in URL capture order for a multi-capture route. No record field IDs or
/// source type serialization are needed.
fn collection_urls(route: &RuntimeWebRoute, provider: &str) -> Result<Vec<String>, String> {
    let label = format!("GET {} collection '{provider}'", route.route.path);
    let plan = prerender_plan(route, &label)?;
    let module = &route.route.handler_module;
    let mut functions = module.debug_functions.iter().filter(|f| f.name == provider);
    let function = functions
        .next()
        .filter(|_| functions.next().is_none())
        .ok_or_else(|| format!("{label}: expected exactly one compiled provider function"))?;
    if !function.params.is_empty() {
        return Err(format!("{label}: provider must accept zero arguments"));
    }
    let mut indices = module
        .function_table
        .iter()
        .enumerate()
        .filter(|(_, offset)| **offset == function.code_offset);
    let (index, _) = indices
        .next()
        .filter(|_| indices.next().is_none())
        .ok_or_else(|| format!("{label}: provider is not a unique callable function"))?;
    prove_handler(module, index)
        .map_err(|reason| format!("{label}: cannot enumerate: {reason}"))?;
    let (module, entry) = prepare_bound_handler(module, index, 0, &[])
        .map_err(|reason| format!("{label}: {reason}"))?;
    let mut vm = VM::new_without_jit();
    vm.load_module(module);
    let value = vm
        .run_from_checked(0, entry, |module, instr| {
            check_instruction(module, instr)
                .map_err(|reason| NuError::runtime_error(reason, Span::default()))
        })
        .map_err(|error| format!("{label}: collection evaluation failed: {error}"))?;
    let rows = vm
        .array_elements(value)
        .ok_or_else(|| format!("{label}: provider must return an array of route keys"))?;
    if rows.is_empty() {
        return Err(format!("{label}: provider returned an empty collection"));
    }
    let capture_count = plan
        .segments
        .iter()
        .filter(|segment| matches!(segment, RuntimeRouteSegment::PathParam(_)))
        .count();
    let mut urls = Vec::with_capacity(rows.len());
    for (index, row) in rows.into_iter().enumerate() {
        let row_label = format!("{label} row {}", index + 1);
        let keys = if capture_count == 1 {
            vec![row]
        } else {
            vm.tuple_elements(row)
                .filter(|keys| keys.len() == capture_count)
                .ok_or_else(|| {
                    format!("{row_label}: expected a tuple of {capture_count} path keys")
                })?
        };
        let mut keys = keys.into_iter();
        let mut url = String::new();
        for segment in &plan.segments {
            url.push('/');
            match segment {
                RuntimeRouteSegment::Literal(literal) => {
                    if literal.contains('/') {
                        return Err(format!("{label}: invalid static route literal"));
                    }
                    url.push_str(literal);
                }
                RuntimeRouteSegment::PathParam(name) => {
                    let ty = plan
                        .bindings
                        .iter()
                        .find(|binding| binding.source_name == *name)
                        .and_then(|binding| binding.ty.as_deref())
                        .ok_or_else(|| format!("{label}: unresolved path binding '{name}'"))?;
                    let key = keys
                        .next()
                        .ok_or_else(|| format!("{row_label}: missing key '{name}'"))?;
                    let key = collection_key(&vm, key, ty)
                        .ok_or_else(|| format!("{row_label}: path key '{name}' must be {ty}"))?;
                    if key.is_empty() || key.contains('/') {
                        return Err(format!(
                            "{row_label}: invalid static route path key '{name}'"
                        ));
                    }
                    url.push_str(&key);
                }
            }
        }
        artifact_path(&url).map_err(|error| format!("{row_label}: {error}"))?;
        urls.push(url);
    }
    Ok(urls)
}

fn collection_key(vm: &VM, value: Value, ty: &str) -> Option<String> {
    match ty {
        "String" => vm.string_operand(0, value),
        "Int" => value.as_int().map(|v| v.to_string()),
        "Bool" => value.as_bool().map(|v| v.to_string()),
        "Float" => value
            .as_float()
            .filter(|v| v.is_finite())
            .map(|v| v.to_string()),
        _ => None,
    }
}

fn reserve_path(
    paths: &mut BTreeMap<String, String>,
    path: &str,
    label: &str,
) -> Result<(), String> {
    let key = path.to_ascii_lowercase();
    let prefix = format!("{key}/");
    let conflict = paths
        .get(&key)
        .or_else(|| {
            key.match_indices('/')
                .find_map(|(i, _)| paths.get(&key[..i]))
        })
        .or_else(|| {
            paths
                .range(prefix.clone()..)
                .next()
                .filter(|(other, _)| other.starts_with(&prefix))
                .map(|(_, owner)| owner)
        });
    if let Some(owner) = conflict {
        return Err(format!(
            "{label}: output path '{path}' conflicts with {owner}"
        ));
    }
    for reserved in ["app.client.js", "app.signals.json", "nulang-app.ir.json"] {
        if key == reserved || key.starts_with(&format!("{reserved}/")) {
            return Err(format!(
                "{label}: output path '{path}' conflicts with {reserved}"
            ));
        }
    }
    paths.insert(key, label.into());
    Ok(())
}

// One proof per route pattern, independent of concrete prerender arguments.
fn prove_candidate(route: &IrRoute, executable: &WebRoute, enumerated: bool) -> Result<(), String> {
    if let Some(reason) = ineligible(route, executable, enumerated) {
        return Err(reason.into());
    }
    prove_handler(&executable.handler_module, executable.handler_func_idx)
}

fn ineligible(route: &IrRoute, executable: &WebRoute, enumerated: bool) -> Option<&'static str> {
    if route.method != "GET" {
        Some("only GET handlers produce static pages")
    } else if enumerated && route.params.is_empty() {
        Some("prerender requires a parameterized route")
    } else if !enumerated
        && (!route.params.is_empty()
            || route.path.contains(':')
            || route.path.contains('{')
            || route.path.contains('}'))
    {
        Some("parameterized routes require explicit static parameter enumeration")
    } else if route.handler.is_none() {
        Some("handler contract is unresolved")
    } else if !enumerated && (!route.handler_params.is_empty() || !route.bindings.is_empty()) {
        Some("handler depends on request arguments")
    } else if !executable
        .handler_module
        .function_table
        .get(executable.handler_func_idx)
        .is_some_and(|offset| {
            executable
                .handler_module
                .debug_functions
                .iter()
                .any(|function| {
                    function.code_offset == *offset
                        && function.params.len() == route.handler_params.len()
                })
        })
    {
        Some("compiled handler arity does not match the static binding plan")
    } else if route
        .response_type
        .as_deref()
        .is_some_and(|ty| !matches!(ty, "Html" | "String"))
    {
        Some("declared response type is not HTML or text")
    } else if route.effects.iter().any(|effect| {
        !matches!(
            effect.as_str(),
            "Web" | "Render" | "Array" | "String" | "Int" | "Float"
        )
    }) {
        Some("handler has declared non-render effects")
    } else {
        None
    }
}

// A deliberately narrow, portable filesystem mapping. Percent-encoded paths,
// traversal, Windows separators/drive prefixes, and empty segments must not
// become filenames. A single trailing slash is normalized.
pub(crate) fn artifact_path(path: &str) -> Result<String, String> {
    let relative = path
        .strip_prefix('/')
        .ok_or_else(|| format!("invalid static route path '{path}'"))?;
    if relative.is_empty() {
        return Ok("index.html".into());
    }
    let relative = relative.strip_suffix('/').unwrap_or(relative);
    if relative.split('/').any(|segment| {
        segment.is_empty()
            || segment == "."
            || segment == ".."
            || segment.ends_with('.')
            || segment
                .bytes()
                .any(|b| !b.is_ascii_alphanumeric() && !matches!(b, b'-' | b'_' | b'.' | b'~'))
            || matches!(
                segment
                    .split('.')
                    .next()
                    .unwrap_or("")
                    .to_ascii_uppercase()
                    .as_str(),
                "CON"
                    | "PRN"
                    | "AUX"
                    | "NUL"
                    | "COM1"
                    | "COM2"
                    | "COM3"
                    | "COM4"
                    | "COM5"
                    | "COM6"
                    | "COM7"
                    | "COM8"
                    | "COM9"
                    | "LPT1"
                    | "LPT2"
                    | "LPT3"
                    | "LPT4"
                    | "LPT5"
                    | "LPT6"
                    | "LPT7"
                    | "LPT8"
                    | "LPT9"
            )
    }) {
        return Err(format!("invalid static route path '{path}'"));
    }
    Ok(format!("{relative}/index.html"))
}

enum Rendered {
    Html(String),
    Denied(String),
}

fn render(route: &WebRoute, arity: usize, args: &[BoundRouteArgument]) -> Result<Rendered, String> {
    let label = format!("{} {}", route.method.as_str(), route.path);
    let (module, entry) =
        prepare_bound_handler(&route.handler_module, route.handler_func_idx, arity, args)
            .map_err(|reason| format!("{label}: {reason}"))?;
    let mut vm = VM::new_without_jit();
    vm.load_module(module);
    let mut denied = None;
    let result = vm.run_from_checked(0, entry, |module, instr| {
        if let Err(reason) = check_instruction(module, instr) {
            denied = Some(reason.clone());
            return Err(NuError::runtime_error(reason, Span::default()));
        }
        Ok(())
    });
    if let Some(reason) = denied {
        return Ok(Rendered::Denied(reason));
    }
    let value = result.map_err(|error| format!("{label}: static render failed: {error}"))?;
    let html = vm
        .string_operand(0, value)
        .ok_or_else(|| format!("{label}: static render did not return a string"))?;
    Ok(Rendered::Html(html))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_paths_are_portable_and_contained() {
        assert_eq!(artifact_path("/").unwrap(), "index.html");
        assert_eq!(artifact_path("/docs/").unwrap(), "docs/index.html");
        for path in [
            "docs",
            "//",
            "/../outside",
            "/a/./b",
            "/a//b",
            "/a\\b",
            "/a%2fb",
            "/C:/tmp",
            "/CON",
        ] {
            assert!(artifact_path(path).is_err(), "{path}");
        }
    }
}
