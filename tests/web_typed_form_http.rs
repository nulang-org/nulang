use nulang::lexer::Lexer;
use nulang::mir_codegen::compile_mir_with_web_field_layout;
use nulang::parser::Parser;
use nulang::runtime::{HttpMethod, WebDevServer, WebRoute};
use nulang::typechecker::TypeChecker;
use nulang::web::contracts::compile_module_contracts;
use nulang::web::form_guard::{CsrfPolicy, GuardedFormRoute};
use nulang::web::forms::{FormContract, FormFieldShape};
use nulang::web::runtime_bindings::{compile_runtime_route_plan, RuntimeWebRoute};
use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream};
use std::time::Duration;

fn compile_typed_guard(
    source: &str,
    shapes: &[(&str, FormFieldShape)],
) -> (RuntimeWebRoute, GuardedFormRoute) {
    let ast = Parser::new(Lexer::new(source).lex().unwrap())
        .parse_module()
        .unwrap();
    let contracts = compile_module_contracts(&ast);
    assert!(
        contracts.diagnostics.is_empty(),
        "{:?}",
        contracts.diagnostics
    );
    let contract = &contracts.routes[0];
    let mut form = FormContract::from_route(contract).unwrap();
    for (name, shape) in shapes {
        form = form.with_shape(name, *shape).unwrap();
    }
    let mut checker = TypeChecker::new();
    checker.check_module(&ast).unwrap();
    let hir = nulang::hir_lower::lower_module(&ast, &checker.inferred_decl_types);
    let mut mir = nulang::mir_lower::lower_module(&hir).unwrap();
    let compiled = compile_mir_with_web_field_layout(&mut mir, "typed-http-form").unwrap();
    let offset = compiled
        .module()
        .debug_functions
        .iter()
        .find(|f| f.name == "endpoint")
        .unwrap()
        .code_offset;
    let func_idx = compiled
        .module()
        .function_table
        .iter()
        .position(|i| *i == offset)
        .unwrap();
    let route = RuntimeWebRoute {
        route: WebRoute {
            method: HttpMethod::Post,
            path: "/leads".into(),
            handler_module: compiled.module().clone(),
            handler_func_idx: func_idx,
        },
        plan: Some(compile_runtime_route_plan(contract).unwrap()),
    };
    let csrf = CsrfPolicy::new("__Host-test-session", [42u8; 32]).unwrap();
    (route, GuardedFormRoute::new_typed(form, csrf, compiled))
}

fn send(port: u16, body: &str, session: Option<&str>) -> (u16, String) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let cookie = session
        .map(|s| format!("Cookie: __Host-test-session={s}\r\n"))
        .unwrap_or_default();
    let req = format!(
        "POST /leads HTTP/1.1\r\nHost: localhost:{port}\r\nOrigin: http://localhost:{port}\r\nContent-Type: application/x-www-form-urlencoded\r\n{cookie}Content-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()
    );
    stream.write_all(req.as_bytes()).unwrap();
    stream.shutdown(Shutdown::Write).unwrap();
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes).unwrap();
    let text = String::from_utf8(bytes).unwrap();
    let status = text
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse::<u16>()
        .unwrap();
    (status, text)
}

const OPTION_SOURCE: &str = r#"
type Option[T] = Some(T) | None
fn endpoint(note: Option[String] from form("note")) -> String {
    match note with { | Some(value) => value | None => "missing" }
}
fn web_main() { perform Web.route("POST", "/leads", endpoint) }
"#;

const ARRAY_SOURCE: &str = r#"
fn endpoint(ages: [Int] from form("ages")) -> Int { ages[0] * 10 + ages[1] }
fn web_main() { perform Web.route("POST", "/leads", endpoint) }
"#;

#[test]
fn http_compiled_option_handlers_execute_some_and_none_without_string_coercion() {
    let (route, guard) = compile_typed_guard(OPTION_SOURCE, &[("note", FormFieldShape::Optional)]);
    let token = guard
        .csrf
        .token_for_session("user-session", "POST", "/leads");
    let server =
        WebDevServer::bind_runtime_with_form_guards(0, None, None, vec![route], vec![guard])
            .unwrap();
    let (status, response) = send(
        server.port,
        &format!("note=hello&__nulang_csrf={token}"),
        Some("user-session"),
    );
    assert_eq!(status, 200, "{response}");
    assert!(response.ends_with("hello"), "{response}");
    let (status, response) = send(
        server.port,
        &format!("__nulang_csrf={token}"),
        Some("user-session"),
    );
    assert_eq!(status, 200, "{response}");
    assert!(response.ends_with("missing"), "{response}");
}

#[test]
fn http_compiled_arrays_preserve_repeated_typed_form_order() {
    let (route, guard) = compile_typed_guard(
        ARRAY_SOURCE,
        &[(
            "ages",
            FormFieldShape::Repeated {
                min_items: 2,
                max_items: 3,
            },
        )],
    );
    let token = guard
        .csrf
        .token_for_session("user-session", "POST", "/leads");
    let server =
        WebDevServer::bind_runtime_with_form_guards(0, None, None, vec![route], vec![guard])
            .unwrap();
    let (status, response) = send(
        server.port,
        &format!("ages=7&%61ges=4&__nulang_csrf={token}"),
        Some("user-session"),
    );
    assert_eq!(status, 200, "{response}");
    assert!(response.ends_with("74"), "{response}");
    let (status, response) = send(
        server.port,
        &format!("ages=7&ages=bogus&__nulang_csrf={token}"),
        Some("user-session"),
    );
    assert_eq!(status, 422, "{response}");
    assert!(response.contains("invalid_type"));
    assert!(!response.contains("bogus"));
}

#[test]
fn typed_http_routes_check_csrf_before_422_and_handler_execution() {
    let (route, guard) = compile_typed_guard(
        ARRAY_SOURCE,
        &[(
            "ages",
            FormFieldShape::Repeated {
                min_items: 2,
                max_items: 3,
            },
        )],
    );
    let token = guard
        .csrf
        .token_for_session("user-session", "POST", "/leads");
    let server =
        WebDevServer::bind_runtime_with_form_guards(0, None, None, vec![route], vec![guard])
            .unwrap();
    let (status, _) = send(server.port, "ages=invalid", Some("user-session"));
    assert_eq!(status, 403);
    let (status, _) = send(
        server.port,
        &format!("ages=7&ages=4&__nulang_csrf={token}"),
        Some("another-session"),
    );
    assert_eq!(status, 403);
}

#[test]
fn typed_registration_rejects_layout_mismatch_and_type_shape_mismatch() {
    let (mut route, guard) =
        compile_typed_guard(OPTION_SOURCE, &[("note", FormFieldShape::Optional)]);
    route.route.handler_module.name.push_str("-mutated");
    assert!(
        WebDevServer::bind_runtime_with_form_guards(0, None, None, vec![route], vec![guard])
            .is_err()
    );

    let (route, guard) = compile_typed_guard(OPTION_SOURCE, &[]);
    assert!(
        WebDevServer::bind_runtime_with_form_guards(0, None, None, vec![route], vec![guard])
            .is_err()
    );

    let (route, guard) = compile_typed_guard(ARRAY_SOURCE, &[("ages", FormFieldShape::Optional)]);
    assert!(
        WebDevServer::bind_runtime_with_form_guards(0, None, None, vec![route], vec![guard])
            .is_err()
    );
}

#[test]
fn typed_registration_rejects_misleading_constraint_metadata() {
    let (route, mut guard) = compile_typed_guard(
        ARRAY_SOURCE,
        &[(
            "ages",
            FormFieldShape::Repeated {
                min_items: 2,
                max_items: 3,
            },
        )],
    );
    guard.form.fields[0].field_type = nulang::web::forms::FormFieldType::Text;
    assert!(
        WebDevServer::bind_runtime_with_form_guards(0, None, None, vec![route], vec![guard])
            .is_err()
    );
}

const OPTIONAL_INT_SOURCE: &str = r#"
type Option[T] = Some(T) | None
fn endpoint(note: Option[Int] from form("note")) -> Int {
    match note with { | Some(value) => value | None => 0 }
}
fn web_main() { perform Web.route("POST", "/leads", endpoint) }
"#;

#[test]
fn typed_optional_int_is_checked_as_int_and_not_string() {
    let (route, guard) =
        compile_typed_guard(OPTIONAL_INT_SOURCE, &[("note", FormFieldShape::Optional)]);
    let token = guard
        .csrf
        .token_for_session("user-session", "POST", "/leads");
    let server =
        WebDevServer::bind_runtime_with_form_guards(0, None, None, vec![route], vec![guard])
            .unwrap();
    let (status, body) = send(
        server.port,
        &format!("note=42&__nulang_csrf={token}"),
        Some("user-session"),
    );
    assert_eq!(status, 200, "{body}");
    assert!(body.ends_with("42"), "{body}");
    let (status, body) = send(
        server.port,
        &format!("__nulang_csrf={token}"),
        Some("user-session"),
    );
    assert_eq!(status, 200, "{body}");
    assert!(body.ends_with("0"), "{body}");
    let (status, body) = send(
        server.port,
        &format!("note=wrong&__nulang_csrf={token}"),
        Some("user-session"),
    );
    assert_eq!(status, 422, "{body}");
    assert!(body.contains("invalid_type"), "{body}");
    assert!(!body.contains("wrong"), "{body}");
}

#[test]
fn typed_route_registration_rejects_forged_handler_type_despite_matching_form_metadata() {
    let (mut route, mut guard) = compile_typed_guard(
        ARRAY_SOURCE,
        &[(
            "ages",
            FormFieldShape::Repeated {
                min_items: 1,
                max_items: 3,
            },
        )],
    );
    route.plan.as_mut().unwrap().bindings[0].ty = Some("[Bool]".into());
    guard.form.fields[0].source_type = Some("[Bool]".into());
    guard.form.fields[0].field_type = nulang::web::forms::FormFieldType::Bool;
    assert!(
        WebDevServer::bind_runtime_with_form_guards(0, None, None, vec![route], vec![guard],)
            .is_err(),
        "compiler-produced MIR signature must win over mutable route metadata",
    );
}

// Integration regression: typed form VM calls must participate in the same
// request-scoped Turbo response marker as ordinary Web handlers.
const TURBO_OPTION_SOURCE: &str = r#"
type Option[T] = Some(T) | None
fn endpoint(note: Option[String] from form("note")) {
    match note with {
        | Some(value) => perform Web.turbo_stream("replace", "flash", perform Web.text(value))
        | None => perform Web.turbo_stream("replace", "flash", perform Web.text("missing"))
    }
}
fn web_main() { perform Web.route("POST", "/leads", endpoint) }
"#;

#[test]
fn typed_form_can_return_guarded_turbo_stream_and_preserve_csrf() {
    let (route, guard) =
        compile_typed_guard(TURBO_OPTION_SOURCE, &[("note", FormFieldShape::Optional)]);
    let token = guard
        .csrf
        .token_for_session("user-session", "POST", "/leads");
    let server =
        WebDevServer::bind_runtime_with_form_guards(0, None, None, vec![route], vec![guard])
            .unwrap();

    let (status, forbidden) = send(server.port, "note=hello", Some("user-session"));
    assert_eq!(status, 403, "{forbidden}");
    assert!(!forbidden.contains("turbo-stream"), "{forbidden}");

    let (status, response) = send(
        server.port,
        &format!("note=hello&__nulang_csrf={token}"),
        Some("user-session"),
    );
    assert_eq!(status, 200, "{response}");
    assert!(
        response.contains("Content-Type: text/vnd.turbo-stream.html; charset=utf-8"),
        "{response}"
    );
    assert!(
        response.contains("<turbo-stream action=\"replace\" target=\"flash\"><template>hello</template></turbo-stream>"),
        "{response}"
    );
    assert!(!response.contains("/app.client.js"), "{response}");

    // The real typed form value must remain escaped inside Turbo's template.
    let (status, hostile) = send(
        server.port,
        &format!("note=%3Cscript%3Ealert(1)%3C%2Fscript%3E&__nulang_csrf={token}"),
        Some("user-session"),
    );
    assert_eq!(status, 200, "{hostile}");
    assert!(
        hostile.contains("<template>&lt;script&gt;alert(1)&lt;/script&gt;</template>"),
        "{hostile}"
    );
    assert!(!hostile.contains("<template><script>"), "{hostile}");
}
