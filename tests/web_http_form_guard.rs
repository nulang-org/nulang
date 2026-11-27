use nulang::bytecode::CodeModule;
use nulang::lexer::Lexer;
use nulang::parser::Parser;
use nulang::runtime::{HttpMethod, WebDevServer, WebRoute};
use nulang::typechecker::TypeChecker;
use nulang::web::contracts::compile_module_contracts;
use nulang::web::form_guard::{CsrfPolicy, GuardedFormRoute};
use nulang::web::forms::{FormContract, FormRule};
use nulang::web::runtime_bindings::{compile_runtime_route_plan, RuntimeWebRoute};
use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream};
use std::time::Duration;

fn compiled_route() -> (RuntimeWebRoute, FormContract) {
    let source = r#"
fn lead(company: String from form("company"), seats: Int from form("seats")) -> String {
    company
}
fn web_main() {
    perform Web.route("POST", "/leads", lead)
}
"#;
    let tokens = Lexer::new(source).lex().unwrap();
    let ast = Parser::new(tokens).parse_module().unwrap();
    let contracts = compile_module_contracts(&ast);
    assert!(
        contracts.diagnostics.is_empty(),
        "{:?}",
        contracts.diagnostics
    );
    let contract = &contracts.routes[0];
    let form = FormContract::from_route(contract)
        .unwrap()
        .with_rules("company", vec![FormRule::MinLength(2)])
        .unwrap()
        .with_rules("seats", vec![FormRule::MinInt(1)])
        .unwrap();

    // The executable is compiled from the same source as the route contract,
    // not hand-constructed runtime bytecode.
    let mut typechecker = TypeChecker::new();
    typechecker.check_module(&ast).unwrap();
    let hir = nulang::hir_lower::lower_module(&ast, &typechecker.inferred_decl_types);
    let mut mir = nulang::mir_lower::lower_module(&hir).unwrap();
    let module: CodeModule =
        nulang::mir_codegen::compile_mir(&mut mir, "web-http-form-guard").unwrap();
    let offset = module
        .debug_functions
        .iter()
        .find(|entry| entry.name == "lead")
        .unwrap()
        .code_offset;
    let function_index = module
        .function_table
        .iter()
        .position(|position| *position == offset)
        .unwrap();

    let route = RuntimeWebRoute {
        route: WebRoute {
            method: HttpMethod::Post,
            path: "/leads".into(),
            handler_module: module,
            handler_func_idx: function_index,
        },
        plan: Some(compile_runtime_route_plan(contract).unwrap()),
    };
    (route, form)
}

fn request(port: u16, body: &str, cookie: Option<&str>) -> (u16, serde_json::Value, String) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let cookie_header = cookie
        .map(|value| format!("Cookie: __Host-test-session={value}\r\n"))
        .unwrap_or_default();
    let request = format!(
        "POST /leads HTTP/1.1\r\nHost: localhost:{port}\r\nOrigin: http://localhost:{port}\r\nContent-Type: application/x-www-form-urlencoded\r\n{cookie_header}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).unwrap();
    stream.shutdown(Shutdown::Write).unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).unwrap();
    let response = String::from_utf8(response).unwrap();
    let (head, response_body) = response.split_once("\r\n\r\n").unwrap();
    let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    let parsed = serde_json::from_str(response_body).unwrap_or(serde_json::Value::Null);
    (status, parsed, response)
}

fn guarded_route() -> (RuntimeWebRoute, GuardedFormRoute) {
    let (route, form) = compiled_route();
    let csrf = CsrfPolicy::new("__Host-test-session", [42u8; 32]).unwrap();
    (route, GuardedFormRoute::new(form, csrf))
}

#[test]
fn guarded_form_rejects_missing_or_bad_csrf_before_handler() {
    let (route, guard) = guarded_route();
    let server = WebDevServer::bind_runtime_with_form_guards(
        0,
        None,
        None,
        vec![route],
        vec![guard.clone()],
    )
    .unwrap();

    let (status, _, _) = request(server.port, "company=Acme&seats=1", Some("user-session"));
    assert_eq!(status, 403);
    let (status, _, _) = request(
        server.port,
        "company=Acme&seats=1&__nulang_csrf=bogus",
        Some("user-session"),
    );
    assert_eq!(status, 403);
    let token = guard
        .csrf
        .token_for_session("user-session", "POST", "/leads");
    let (status, _, _) = request(
        server.port,
        &format!("company=Acme&seats=1&__nulang_csrf={token}"),
        None,
    );
    assert_eq!(status, 403);
    let (status, _, _) = request(
        server.port,
        &format!("company=Acme&seats=1&__nulang_csrf={token}&__nulang_csrf={token}"),
        Some("user-session"),
    );
    assert_eq!(status, 403);
    let (status, _, _) = request(
        server.port,
        &format!("company=Acme&seats=1&__nulang_csrf={token}"),
        Some("different-session"),
    );
    assert_eq!(status, 403);
}

#[test]
fn guarded_form_returns_422_field_errors_on_invalid_semantics_and_200_on_valid() {
    let (route, guard) = guarded_route();
    let token = guard
        .csrf
        .token_for_session("user-session", "POST", "/leads");
    let server =
        WebDevServer::bind_runtime_with_form_guards(0, None, None, vec![route], vec![guard])
            .unwrap();

    let (status, problem, response) = request(
        server.port,
        &format!("company=x&seats=0&__nulang_csrf={token}"),
        Some("user-session"),
    );
    assert_eq!(status, 422, "{response}");
    assert!(response
        .to_ascii_lowercase()
        .contains("content-type: application/problem+json"));
    assert_eq!(problem["errors"][0]["field"], "company");
    assert_eq!(problem["errors"][0]["code"], "too_short");
    assert_eq!(problem["errors"][1]["field"], "seats");
    assert_eq!(problem["errors"][1]["code"], "too_small");

    let (status, problem, response) = request(
        server.port,
        &format!("company=Acme&seats=5&__nulang_csrf={token}"),
        Some("user-session"),
    );
    assert_eq!(status, 200, "{response}");
    assert!(response.ends_with("Acme"), "{response}");
    assert!(problem.is_null());
}

#[test]
fn guard_registration_rejects_mismatched_route_contract() {
    let (route, mut guard) = guarded_route();
    guard.form.fields[0].name = "mismatched".into();
    assert!(
        WebDevServer::bind_runtime_with_form_guards(0, None, None, vec![route], vec![guard],)
            .is_err()
    );

    let (_route, guard) = guarded_route();
    assert!(
        WebDevServer::bind_runtime_with_form_guards(0, None, None, vec![], vec![guard],).is_err()
    );
}

#[test]
fn csrf_token_is_session_route_and_method_bound() {
    let csrf = CsrfPolicy::new("__Host-test-session", [42u8; 32]).unwrap();
    let token = csrf.token_for_session("session-a", "POST", "/leads");
    assert_eq!(token.len(), 64);
    assert_ne!(token, csrf.token_for_session("session-b", "POST", "/leads"));
    assert_ne!(token, csrf.token_for_session("session-a", "POST", "/other"));
    assert_ne!(token, csrf.token_for_session("session-a", "PUT", "/leads"));
    assert!(CsrfPolicy::new("unscoped-session", [42u8; 32]).is_err());
}

#[test]
fn duplicate_session_cookie_names_are_rejected_without_last_value_selection() {
    let (route, guard) = guarded_route();
    let token = guard
        .csrf
        .token_for_session("user-session", "POST", "/leads");
    let server =
        WebDevServer::bind_runtime_with_form_guards(0, None, None, vec![route], vec![guard])
            .unwrap();
    let (status, _, _) = request(
        server.port,
        &format!("company=Acme&seats=1&__nulang_csrf={token}"),
        Some("user-session; __Host-test-session=user-session"),
    );
    assert_eq!(status, 403);
}

#[test]
fn guard_rejects_mutated_rules_and_reserved_field_names_before_startup() {
    let (route, mut guard) = guarded_route();
    guard.form.fields[0].rules = vec![FormRule::MinInt(2)];
    assert!(
        WebDevServer::bind_runtime_with_form_guards(0, None, None, vec![route], vec![guard],)
            .is_err()
    );

    let (route, mut guard) = guarded_route();
    guard.form.fields[0].source_type = Some("Bool".into());
    assert!(
        WebDevServer::bind_runtime_with_form_guards(0, None, None, vec![route], vec![guard],)
            .is_err()
    );
}

#[test]
fn guard_rejects_duplicate_route_policy_registration() {
    let (route, guard) = guarded_route();
    assert!(WebDevServer::bind_runtime_with_form_guards(
        0,
        None,
        None,
        vec![route],
        vec![guard.clone(), guard],
    )
    .is_err());
}

#[test]
fn csrf_constructor_rejects_guessable_zero_key() {
    assert!(CsrfPolicy::new("__Host-test-session", [0u8; 32]).is_err());
}

#[test]
fn csrf_signing_key_is_redacted_from_debug_output() {
    let policy = CsrfPolicy::new("__Host-test-session", [42u8; 32]).unwrap();
    let debug = format!("{policy:?}");
    assert!(debug.contains("[redacted]"), "{debug}");
    assert!(!debug.contains("42, 42"), "{debug}");
}

#[test]
fn multiple_runtime_routes_cannot_share_an_ambiguous_form_guard() {
    let (route, guard) = guarded_route();
    assert!(WebDevServer::bind_runtime_with_form_guards(
        0,
        None,
        None,
        vec![route.clone(), route],
        vec![guard],
    )
    .is_err());
}

#[test]
fn optional_and_repeated_host_fields_cannot_be_bound_as_scalar_vm_handler_arguments() {
    use nulang::web::forms::FormFieldShape;
    let (route, mut guard) = guarded_route();
    guard.form = guard
        .form
        .with_shape("company", FormFieldShape::Optional)
        .unwrap();
    assert!(
        WebDevServer::bind_runtime_with_form_guards(0, None, None, vec![route], vec![guard],)
            .is_err()
    );

    let (route, mut guard) = guarded_route();
    guard.form = guard
        .form
        .with_shape(
            "seats",
            FormFieldShape::Repeated {
                min_items: 1,
                max_items: 3,
            },
        )
        .unwrap();
    assert!(
        WebDevServer::bind_runtime_with_form_guards(0, None, None, vec![route], vec![guard],)
            .is_err()
    );
}

#[test]
fn structured_handler_annotation_is_not_coerced_to_vm_string() {
    let (mut route, mut guard) = guarded_route();
    let binding = &mut route.plan.as_mut().unwrap().bindings[0];
    binding.ty = Some("Option[String]".into());
    guard.form.fields[0].source_type = Some("Option[String]".into());
    // Host-side metadata remains string-backed, but cannot execute this
    // form as a typed VM route without real Option variant construction.
    assert!(
        WebDevServer::bind_runtime_with_form_guards(0, None, None, vec![route], vec![guard],)
            .is_err()
    );
}

#[test]
fn guarded_form_with_valid_csrf_rejects_untrusted_origin_and_host() {
    // This regression intentionally exercises the composed security and
    // typed-form pipelines, not merely the independent form validator.
    let (route, guard) = guarded_route();
    let token = guard
        .csrf
        .token_for_session("user-session", "POST", "/leads");
    let server =
        WebDevServer::bind_runtime_with_form_guards(0, None, None, vec![route], vec![guard])
            .unwrap();
    let body = format!("company=Acme&seats=2&__nulang_csrf={token}");

    let send = |host: &str, extra_headers: &str| {
        let mut stream = TcpStream::connect(("127.0.0.1", server.port)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let request = format!(
            "POST /leads HTTP/1.1\r\nHost: {host}\r\n{extra_headers}Content-Type: application/x-www-form-urlencoded\r\nCookie: __Host-test-session=user-session\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(request.as_bytes()).unwrap();
        stream.shutdown(Shutdown::Write).unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response
    };

    let local_host = format!("localhost:{}", server.port);
    let local_origin = format!("Origin: http://localhost:{}\r\n", server.port);
    let foreign_origin = send(&local_host, "Origin: https://attacker.example\r\n");
    assert!(
        foreign_origin.starts_with("HTTP/1.1 403 Forbidden"),
        "foreign origin must be rejected before a valid guarded form executes: {foreign_origin}"
    );

    let foreign_host = send("attacker.example", &local_origin);
    assert!(
        foreign_host.starts_with("HTTP/1.1 403 Forbidden"),
        "foreign Host must be rejected before a valid guarded form executes: {foreign_host}"
    );

    let (status, _, response) = request(server.port, &body, Some("user-session"));
    assert_eq!(
        status, 200,
        "the same valid form should execute without hostile request provenance: {response}"
    );
}
