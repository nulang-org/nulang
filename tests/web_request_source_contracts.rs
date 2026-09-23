use nulang::ast::{FunctionAnnotation, WebRequestParamSource};
use nulang::lexer::Lexer;
use nulang::parser::Parser;
use nulang::web::bindings::{compile_route_bindings, RouteBindingSource};
use nulang::web::contracts::{compile_module_contracts, RequestParamSource};

fn parse(source: &str) -> nulang::ast::AstModule {
    let tokens = Lexer::new(source).lex().unwrap();
    Parser::new(tokens).parse_module().unwrap()
}

#[test]
fn contextual_request_sources_preserve_parameter_types_and_lower_to_bindings() {
    let module = parse(
        r#"
fn endpoint(
    id: Int from path("user_id"),
    limit: Int from query,
    trace: String from header("X-Trace"),
    session: String from cookie("session"),
    payload: String from body,
    title: String from form("title")
) -> String { "ok" }

fn web_main() {
    perform Web.route("POST", "/users/{user_id: Int}", endpoint)
}
"#,
    );

    let function = module
        .decls
        .iter()
        .find(|decl| matches!(decl, nulang::ast::Decl::Function { name, .. } if name == "endpoint"))
        .unwrap();
    if let nulang::ast::Decl::Function {
        params,
        annotations,
        ..
    } = function
    {
        assert_eq!(params[0].ty.as_ref().unwrap().to_string(), "Int");
        assert!(annotations.iter().any(|annotation| matches!(
            annotation,
            FunctionAnnotation::RequestBinding {
                param,
                source: WebRequestParamSource::Query,
                source_name,
            } if param == "limit" && source_name == "limit"
        )));
    }

    let compiled = compile_module_contracts(&module);
    assert!(
        compiled.diagnostics.is_empty(),
        "{:?}",
        compiled.diagnostics
    );
    let route = &compiled.routes[0];
    assert_eq!(
        route.handler_params[0].request.as_ref().unwrap().source,
        RequestParamSource::Path
    );
    assert_eq!(
        route.handler_params[1].request.as_ref().unwrap().source,
        RequestParamSource::Query
    );

    let bindings = compile_route_bindings(route);
    assert!(
        bindings.diagnostics.is_empty(),
        "{:?}",
        bindings.diagnostics
    );
    assert_eq!(bindings.bindings.len(), 6);
    assert_eq!(bindings.bindings[0].source, RouteBindingSource::Path);
    assert_eq!(bindings.bindings[0].source_name, "user_id");
    assert_eq!(bindings.bindings[1].source, RouteBindingSource::Query);
    assert_eq!(bindings.bindings[2].source, RouteBindingSource::Header);
    assert_eq!(bindings.bindings[3].source, RouteBindingSource::Cookie);
    assert_eq!(bindings.bindings[4].source, RouteBindingSource::Body);
    assert_eq!(bindings.bindings[5].source, RouteBindingSource::Form);
}

#[test]
fn unknown_request_source_is_a_parse_error() {
    let tokens = Lexer::new("fn endpoint(limit: Int from matrix) { limit }")
        .lex()
        .unwrap();
    let error = Parser::new(tokens).parse_module().unwrap_err();
    assert!(error
        .to_string()
        .contains("unknown request source 'matrix'"));
}
