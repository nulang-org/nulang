use nulang::bytecode::Constant;
use nulang::lexer::Lexer;
use nulang::parser::Parser;
use nulang::web::bindings::compile_route_bindings;
use nulang::web::contracts::compile_module_contracts;
use nulang::web::http_request::HttpRequestBindingInputs;
use nulang::web::request_bindings::bind_request_arguments;
use std::collections::HashMap;

fn parse(source: &str) -> nulang::ast::AstModule {
    let tokens = Lexer::new(source).lex().unwrap();
    Parser::new(tokens).parse_module().unwrap()
}

#[test]
fn source_contract_http_capture_and_scalar_decode_share_one_binding_plan() {
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

    let contracts = compile_module_contracts(&module);
    assert!(
        contracts.diagnostics.is_empty(),
        "{:?}",
        contracts.diagnostics
    );
    assert_eq!(contracts.routes.len(), 1);

    let binding_compilation = compile_route_bindings(&contracts.routes[0]);
    assert!(
        binding_compilation.diagnostics.is_empty(),
        "{:?}",
        binding_compilation.diagnostics
    );
    assert_eq!(binding_compilation.bindings.len(), 6);

    let path = HashMap::from([("user_id".to_string(), "42".to_string())]);
    let headers = vec![
        ("X-Trace".to_string(), "trace-123".to_string()),
        ("Cookie".to_string(), "session=s1; theme=dark".to_string()),
        (
            "Content-Type".to_string(),
            "application/x-www-form-urlencoded; charset=utf-8".to_string(),
        ),
    ];
    let captured =
        HttpRequestBindingInputs::capture("/users/42?limit=25", &headers, b"title=hello+world");
    let values = captured.values(&path, &headers);

    let arguments = bind_request_arguments(
        &binding_compilation.bindings,
        contracts.routes[0].handler_params.len(),
        &values,
    )
    .unwrap();

    assert_eq!(arguments.len(), 6);
    assert_eq!(arguments[0].value, Constant::Int(42));
    assert_eq!(arguments[1].value, Constant::Int(25));
    assert_eq!(
        arguments[2].value,
        Constant::String("trace-123".to_string())
    );
    assert_eq!(arguments[3].value, Constant::String("s1".to_string()));
    assert_eq!(
        arguments[4].value,
        Constant::String("title=hello+world".to_string())
    );
    assert_eq!(
        arguments[5].value,
        Constant::String("hello world".to_string())
    );
}

#[test]
fn non_form_media_type_leaves_form_binding_missing() {
    let module = parse(
        r#"
fn endpoint(title: String from form("title")) -> String { title }

fn web_main() {
    perform Web.route("POST", "/users", endpoint)
}
"#,
    );
    let contracts = compile_module_contracts(&module);
    let bindings = compile_route_bindings(&contracts.routes[0]);
    assert!(
        bindings.diagnostics.is_empty(),
        "{:?}",
        bindings.diagnostics
    );

    let path = HashMap::new();
    let headers = vec![("Content-Type".to_string(), "application/json".to_string())];
    let captured = HttpRequestBindingInputs::capture("/users", &headers, br#"{"title":"hello"}"#);
    let values = captured.values(&path, &headers);

    let error = bind_request_arguments(&bindings.bindings, 1, &values).unwrap_err();
    assert!(error.is_client_error());
    assert_eq!(error.http_status(), 400);
    assert_eq!(error.code(), "missing_request_input");
}
