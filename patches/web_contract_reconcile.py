from pathlib import Path

contracts = Path("src/web/contracts.rs")
text = contracts.read_text()

old = '''    let functions: HashMap<String, FunctionMeta> = module
        .decls
        .iter()
        .filter_map(FunctionMeta::from_decl)
        .collect();

    let mut raw_routes = Vec::new();'''
new = '''    let functions: HashMap<String, FunctionMeta> = module
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

    let mut raw_routes = Vec::new();'''
if old not in text:
    raise SystemExit("function table marker not found")
text = text.replace(old, new, 1)

old = '''                &mut params,
                &meta.params,
                &mut out.diagnostics,'''
new = '''                &mut params,
                &meta.params,
                &transparent_aliases,
                &mut out.diagnostics,'''
if old not in text:
    raise SystemExit("validation call marker not found")
text = text.replace(old, new, 1)

old = '''    route_params: &mut [RouteParamContract],
    handler_params: &[Param],
    diagnostics: &mut Vec<String>,'''
new = '''    route_params: &mut [RouteParamContract],
    handler_params: &[Param],
    transparent_aliases: &HashMap<String, String>,
    diagnostics: &mut Vec<String>,'''
if old not in text:
    raise SystemExit("validation signature marker not found")
text = text.replace(old, new, 1)

old = '''        match (&route_param.ty, handler_ty) {
            (Some(route_ty), Some(handler_ty)) if route_ty != &handler_ty => {'''
new = '''        match (&route_param.ty, handler_ty) {
            (Some(route_ty), Some(handler_ty))
                if canonical_type_name(route_ty, transparent_aliases) != handler_ty =>
            {'''
if old not in text:
    raise SystemExit("validation comparison marker not found")
text = text.replace(old, new, 1)

marker = '''fn handler_param_contract(param: &Param) -> HandlerParamContract {'''
helper = '''fn canonical_type_name(name: &str, aliases: &HashMap<String, String>) -> String {
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

'''
if marker not in text:
    raise SystemExit("handler contract marker not found")
text = text.replace(marker, helper + marker, 1)

text = text.replace(
    'assert_eq!(route.params[0].ty.as_deref(), Some("UserId"));',
    'assert_eq!(route.params[0].ty.as_deref(), Some("String"));',
    1,
)
text = text.replace(
    'type ExternalId = String\n\nfn show_user(id: UserId)',
    'type ExternalId = Int\n\nfn show_user(id: UserId)',
    1,
)
text = text.replace(
    'assert!(compiled.diagnostics[0].contains("declares UserId"));',
    'assert!(compiled.diagnostics[0].contains("declares String"));',
    1,
)
contracts.write_text(text)

package = Path("src/web/package_contracts.rs")
text = package.read_text()
text = text.replace(
    'fn route(path, handler) { nil }\nfn handler() { nil }\nfn main() { route("/not-web", handler) }',
    'fn route(path, callback) { nil }\nfn callback() { nil }\nfn main() { route("/not-web", callback) }',
)
text = text.replace(
    'fn route(path, handler) { nil }\nfn local_handler() { nil }',
    'fn route(path, callback) { nil }\nfn local_handler() { nil }',
)
package.write_text(text)
