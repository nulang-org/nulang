//! Contract-driven, non-executing request preflight (Precognition-style).
//! It deliberately does not expose a public HTTP route or authorize mutations.

use nulang::bytecode::Constant;
use nulang::lexer::Lexer;
use nulang::parser::Parser;
use nulang::web::bindings::{compile_route_bindings, RouteBindingContract};
use nulang::web::contracts::compile_module_contracts;
use nulang::web::http_request::HttpRequestBindingInputs;
use nulang::web::request_bindings::{
    bind_request_arguments, preflight_request_arguments, RequestDecodeErrorKind,
};
use std::collections::HashMap;

fn compiled_form() -> (Vec<RouteBindingContract>, usize) {
    let source = r#"
fn submit(
    id: Int from path("id"),
    internal_quantity_slot: Int from form("quantity"),
    expedited: Bool from form("expedited")
) -> String { "would mutate" }

fn web_main() {
    perform Web.route("POST", "/orders/{id: Int}", submit)
}
"#;
    let module = Parser::new(Lexer::new(source).lex().unwrap())
        .parse_module()
        .unwrap();
    let compilation = compile_module_contracts(&module);
    assert!(
        compilation.diagnostics.is_empty(),
        "{:?}",
        compilation.diagnostics
    );
    let route = &compilation.routes[0];
    let binding = compile_route_bindings(route);
    assert!(binding.diagnostics.is_empty(), "{:?}", binding.diagnostics);
    (binding.bindings, route.handler_params.len())
}

#[test]
fn valid_preflight_uses_exact_handler_binding_contract_without_execution() {
    let (bindings, param_count) = compiled_form();
    let path = HashMap::from([("id".to_string(), "42".to_string())]);
    let headers = vec![(
        "Content-Type".to_string(),
        "application/x-www-form-urlencoded".to_string(),
    )];
    let captured =
        HttpRequestBindingInputs::capture("/orders/42", &headers, b"quantity=3&expedited=true");
    let values = captured.values(&path, &headers);

    let result = preflight_request_arguments(&bindings, param_count, &values).unwrap();
    assert!(result.valid());
    assert!(result.errors.is_empty());
    assert_eq!(result.to_json()["valid"], true);
    assert_eq!(result.to_json()["errors"].as_array().unwrap().len(), 0);

    // The actual request binds with the same contract; no handler was called.
    let args = bind_request_arguments(&bindings, param_count, &values).unwrap();
    assert_eq!(args[0].value, Constant::Int(42));
    assert_eq!(args[1].value, Constant::Int(3));
    assert_eq!(args[2].value, Constant::Bool(true));
}

#[test]
fn preflight_collects_multiple_field_errors_without_leaking_handler_slots() {
    let (bindings, param_count) = compiled_form();
    let path = HashMap::from([("id".to_string(), "42".to_string())]);
    let headers = vec![(
        "Content-Type".to_string(),
        "application/x-www-form-urlencoded".to_string(),
    )];
    let captured = HttpRequestBindingInputs::capture(
        "/orders/42",
        &headers,
        b"quantity=not-a-number&expedited=perhaps",
    );
    let values = captured.values(&path, &headers);

    let result = preflight_request_arguments(&bindings, param_count, &values).unwrap();
    assert!(!result.valid());
    assert_eq!(result.errors.len(), 2);
    assert_eq!(result.errors[0].kind, RequestDecodeErrorKind::InvalidType);
    assert_eq!(result.errors[1].kind, RequestDecodeErrorKind::InvalidType);
    let json = result.to_json();
    assert_eq!(json["valid"], false);
    assert_eq!(json["errors"][0]["source"], "form");
    assert_eq!(json["errors"][0]["source_name"], "quantity");
    assert_eq!(json["errors"][1]["source_name"], "expedited");
    assert!(json["errors"][0].get("handler_index").is_none());
    assert!(json["errors"][0].get("handler_param").is_none());
    assert!(!json.to_string().contains("internal_quantity_slot"));
}

#[test]
fn missing_form_value_is_reported_without_invocation() {
    let (bindings, param_count) = compiled_form();
    let path = HashMap::from([("id".to_string(), "42".to_string())]);
    let headers = vec![("Content-Type".to_string(), "application/json".to_string())];
    let captured = HttpRequestBindingInputs::capture(
        "/orders/42",
        &headers,
        br#"{"quantity":3,"expedited":true}"#,
    );
    let values = captured.values(&path, &headers);
    let preflight = preflight_request_arguments(&bindings, param_count, &values).unwrap();
    assert_eq!(preflight.errors.len(), 2);
    assert_eq!(preflight.errors[0].kind, RequestDecodeErrorKind::Missing);
    assert_eq!(preflight.errors[1].kind, RequestDecodeErrorKind::Missing);
}

#[test]
fn malformed_plan_is_server_error_even_when_client_input_is_invalid() {
    let (mut bindings, param_count) = compiled_form();
    bindings[1].handler_index = bindings[0].handler_index;
    let empty = HashMap::new();
    let headers = Vec::new();
    let captured = HttpRequestBindingInputs::capture("/orders", &headers, b"");
    let values = captured.values(&empty, &headers);

    let error = preflight_request_arguments(&bindings, param_count, &values).unwrap_err();
    assert_eq!(error.kind, RequestDecodeErrorKind::DuplicateHandlerSlot);
    assert_eq!(error.http_status(), 500);

    bindings[1].handler_index = 99;
    let error = preflight_request_arguments(&bindings, param_count, &values).unwrap_err();
    assert_eq!(error.kind, RequestDecodeErrorKind::InvalidHandlerSlot);
    assert_eq!(error.http_status(), 500);

    bindings.remove(2);
    bindings[1].handler_index = 1;
    let error = preflight_request_arguments(&bindings, param_count, &values).unwrap_err();
    assert_eq!(error.kind, RequestDecodeErrorKind::IncompleteBindingPlan);
    assert_eq!(error.http_status(), 500);

    bindings.clear();
    let error = preflight_request_arguments(&bindings, 256, &values).unwrap_err();
    assert_eq!(error.kind, RequestDecodeErrorKind::InvalidHandlerSlot);
}
