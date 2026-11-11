use nulang::bytecode::CodeModule;
use nulang::lexer::Lexer;
use nulang::parser::Parser;
use nulang::runtime::{HttpMethod, WebDevServer, WebRoute};
use nulang::typechecker::TypeChecker;
use nulang::web::contracts::compile_module_contracts;
use nulang::web::runtime_bindings::{compile_runtime_route_plan, RuntimeWebRoute};
use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream};
use std::time::Duration;

fn compile_handler(source: &str, handler_name: &str) -> (CodeModule, usize) {
    let tokens = Lexer::new(source).lex().expect("lex handler source");
    let ast = Parser::new(tokens)
        .parse_module()
        .expect("parse handler source");
    let mut type_checker = TypeChecker::new();
    type_checker
        .check_module(&ast)
        .expect("typecheck handler source");
    let hir = nulang::hir_lower::lower_module(&ast, &type_checker.inferred_decl_types);
    let mut mir = nulang::mir_lower::lower_module(&hir).expect("lower handler MIR");
    let module = nulang::mir_codegen::compile_mir(&mut mir, "web-http-test")
        .expect("compile handler bytecode");

    let code_offset = module
        .debug_functions
        .iter()
        .find(|function| function.name == handler_name)
        .unwrap_or_else(|| panic!("missing debug entry for {handler_name}"))
        .code_offset;
    let function_index = module
        .function_table
        .iter()
        .position(|offset| *offset == code_offset)
        .unwrap_or_else(|| panic!("missing function-table entry for {handler_name}"));

    (module, function_index)
}

fn typed_query_route() -> RuntimeWebRoute {
    let (module, function_index) =
        compile_handler("fn endpoint(limit: Int) -> Int { limit }", "endpoint");
    let contract_source = r#"
fn endpoint(limit: Int from query) -> Int { limit }

fn web_main() {
    perform Web.route("GET", "/users", endpoint)
}
"#;
    let tokens = Lexer::new(contract_source)
        .lex()
        .expect("lex route contract source");
    let ast = Parser::new(tokens)
        .parse_module()
        .expect("parse route contract source");
    let contracts = compile_module_contracts(&ast);
    assert!(
        contracts.diagnostics.is_empty(),
        "{:?}",
        contracts.diagnostics
    );
    assert_eq!(contracts.routes.len(), 1);
    let plan =
        compile_runtime_route_plan(&contracts.routes[0]).expect("compile typed runtime route plan");

    RuntimeWebRoute {
        route: WebRoute {
            method: HttpMethod::Get,
            path: "/users".to_string(),
            handler_module: module,
            handler_func_idx: function_index,
        },
        plan: Some(plan),
    }
}

fn legacy_route() -> WebRoute {
    let (module, function_index) =
        compile_handler("fn legacy() -> String { \"legacy\" }", "legacy");
    WebRoute {
        method: HttpMethod::Get,
        path: "/legacy".to_string(),
        handler_module: module,
        handler_func_idx: function_index,
    }
}

fn turbo_route() -> WebRoute {
    let (module, function_index) = compile_handler(
        r#"fn turbo() {
    perform Web.turbo_stream("append", "todos", perform Web.text("Tea & toast </template><script>alert(1)</script>"))
}"#,
        "turbo",
    );
    WebRoute {
        method: HttpMethod::Post,
        path: "/todos".to_string(),
        handler_module: module,
        handler_func_idx: function_index,
    }
}

fn send_raw_request(port: u16, request: &str) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect to WebDevServer");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set read timeout");
    stream
        .write_all(request.as_bytes())
        .expect("write HTTP request");
    stream.shutdown(Shutdown::Write).expect("finish request");

    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .expect("read HTTP response");
    String::from_utf8(response).expect("HTTP response is UTF-8")
}

fn response_body(response: &str) -> &str {
    response
        .split_once("\r\n\r\n")
        .expect("HTTP response separator")
        .1
}

#[test]
fn compiler_bound_query_executes_over_real_http() {
    let server = WebDevServer::bind_runtime(0, None, None, vec![typed_query_route()])
        .expect("bind typed WebDevServer");

    let response = send_raw_request(
        server.port,
        "GET /users?limit=25 HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );

    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    assert!(response_body(&response).starts_with("25"), "{response}");
}

#[test]
fn absolute_form_routes_a_typed_query_over_real_http() {
    let server = WebDevServer::bind_runtime(0, None, None, vec![typed_query_route()])
        .expect("bind typed WebDevServer");

    // The absolute request-target's authority overrides the mismatched Host,
    // and the normalized path/query must reach compiler-backed dispatch.
    let response = send_raw_request(
        server.port,
        "GET http://example.test/users?limit=31 HTTP/1.1\r\nHost: misleading.test\r\nConnection: close\r\n\r\n",
    );

    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    assert!(response_body(&response).starts_with("31"), "{response}");
}

#[test]
fn invalid_typed_query_is_problem_json_over_real_http() {
    let server = WebDevServer::bind_runtime(0, None, None, vec![typed_query_route()])
        .expect("bind typed WebDevServer");

    let response = send_raw_request(
        server.port,
        "GET /users?limit=many HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );

    assert!(
        response.starts_with("HTTP/1.1 400 Bad Request\r\n"),
        "{response}"
    );
    assert!(
        response
            .to_ascii_lowercase()
            .contains("content-type: application/problem+json\r\n"),
        "{response}"
    );
    let problem: serde_json::Value =
        serde_json::from_str(response_body(&response)).expect("parse problem JSON");
    assert_eq!(problem["status"], 400);
    assert_eq!(problem["code"], "invalid_request_input");
    assert_eq!(problem["source"], "query");
    assert_eq!(problem["source_name"], "limit");
}

#[test]
fn legacy_route_still_falls_back_over_real_http() {
    let server =
        WebDevServer::bind(0, None, None, vec![legacy_route()]).expect("bind legacy WebDevServer");

    let response = send_raw_request(
        server.port,
        "GET /legacy HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );

    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    assert!(response_body(&response).starts_with("legacy"), "{response}");
}

fn html_src_route() -> RuntimeWebRoute {
    let (module, function_index) = compile_handler(
        r#"fn endpoint(src: String) {
    perform Web.html("turbo-frame", [
        ("id", perform Web.text("activities")),
        ("src", perform Web.text(src))
    ], [])
}"#,
        "endpoint",
    );
    let contract_source = r#"
fn endpoint(src: String from query) {
    perform Web.html("turbo-frame", [
        ("id", perform Web.text("activities")),
        ("src", perform Web.text(src))
    ], [])
}
fn web_main() { perform Web.route("GET", "/frame", endpoint) }
"#;
    let tokens = Lexer::new(contract_source).lex().expect("lex contract");
    let ast = Parser::new(tokens).parse_module().expect("parse contract");
    let contracts = compile_module_contracts(&ast);
    assert!(
        contracts.diagnostics.is_empty(),
        "{:?}",
        contracts.diagnostics
    );
    let plan = compile_runtime_route_plan(&contracts.routes[0]).expect("compile plan");
    RuntimeWebRoute {
        route: WebRoute {
            method: HttpMethod::Get,
            path: "/frame".into(),
            handler_module: module,
            handler_func_idx: function_index,
        },
        plan: Some(plan),
    }
}

#[test]
fn rejected_html_in_legacy_route_is_http_500_not_successful_nil() {
    let (module, handler_func_idx) = compile_handler(
        r#"fn invalid() {
    perform Web.html("turbo-frame", [
        ("id", perform Web.text("activities")),
        ("src", perform Web.text("javascript:alert(1)"))
    ], [])
}"#,
        "invalid",
    );
    let server = WebDevServer::bind(
        0,
        None,
        None,
        vec![
            WebRoute {
                method: HttpMethod::Get,
                path: "/invalid".into(),
                handler_module: module,
                handler_func_idx,
            },
            legacy_route(),
        ],
    )
    .expect("bind");
    let invalid = send_raw_request(
        server.port,
        "GET /invalid HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    assert!(
        invalid.starts_with("HTTP/1.1 500 Internal Server Error\r\n"),
        "{invalid}"
    );
    assert_eq!(response_body(&invalid), "Internal server error");
    assert!(!invalid.contains("javascript:alert(1)"));

    let valid = send_raw_request(
        server.port,
        "GET /legacy HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    assert!(valid.starts_with("HTTP/1.1 200 OK\r\n"), "{valid}");
    assert!(response_body(&valid).starts_with("legacy"));
}

#[test]
fn nil_string_body_is_not_mistaken_for_vm_nil() {
    let (module, handler_func_idx) =
        compile_handler(r#"fn literal_nil() -> String { "nil" }"#, "literal_nil");
    let server = WebDevServer::bind(
        0,
        None,
        None,
        vec![WebRoute {
            method: HttpMethod::Get,
            path: "/literal".into(),
            handler_module: module,
            handler_func_idx,
        }],
    )
    .expect("bind");
    let response = send_raw_request(
        server.port,
        "GET /literal HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    assert!(response_body(&response).starts_with("nil"), "{response}");
}

#[test]
fn rejected_html_in_compiler_bound_route_is_http_500_not_successful_nil() {
    let server = WebDevServer::bind_runtime(0, None, None, vec![html_src_route()])
        .expect("bind typed WebDevServer");
    let invalid = send_raw_request(
        server.port,
        "GET /frame?src=javascript%3Aalert%281%29 HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    assert!(
        invalid.starts_with("HTTP/1.1 500 Internal Server Error\r\n"),
        "{invalid}"
    );
    assert_eq!(response_body(&invalid), "Internal server error");
    assert!(!invalid.contains("javascript:alert(1)"));

    let valid = send_raw_request(
        server.port,
        "GET /frame?src=%2Factivities HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    assert!(valid.starts_with("HTTP/1.1 200 OK\r\n"), "{valid}");
    assert!(response_body(&valid).starts_with("<turbo-frame"), "{valid}");
    assert!(
        response_body(&valid).contains("src=\"/activities\""),
        "{valid}"
    );
}

#[test]
fn rejected_html_in_path_bound_handler_is_execution_error_not_rendered_nil() {
    use nulang::web::bindings::{RouteBindingContract, RouteBindingSource};
    use nulang::web::runtime_bindings::render_bound_route_handler;
    use std::collections::HashMap;

    let (module, index) = compile_handler(
        r#"fn endpoint(src: String) {
    perform Web.html("turbo-frame", [
        ("id", perform Web.text("activities")),
        ("src", perform Web.text(src))
    ], [])
}"#,
        "endpoint",
    );
    let binding = RouteBindingContract {
        source: RouteBindingSource::Path,
        source_name: "src".into(),
        handler_param: "src".into(),
        handler_index: 0,
        ty: Some("String".into()),
    };
    let mut captures = HashMap::new();
    captures.insert("src".into(), "javascript:alert(1)".into());
    let error = render_bound_route_handler(&module, index, &[binding.clone()], 1, &captures)
        .expect_err("a VM nil is not a renderable response");
    assert!(error.contains("returned nil"), "{error}");
    captures.insert("src".into(), "/activities".into());
    let html = render_bound_route_handler(&module, index, &[binding], 1, &captures)
        .expect("valid Html must render");
    assert!(html.starts_with("<turbo-frame"), "{html}");
    assert!(html.contains("src=\"/activities\""), "{html}");
}

#[test]
fn turbo_stream_web_effect_sets_mime_and_preserves_escaped_html_over_http() {
    let server = WebDevServer::bind(0, None, None, vec![turbo_route(), legacy_route()])
        .expect("bind Turbo WebDevServer");
    // The audited Web ingress requires an exact loopback Host and matching
    // Origin for every browser mutation, including Turbo form submissions.
    let host = format!("localhost:{}", server.port);
    let stream_request = format!(
        "POST /todos HTTP/1.1\r\nHost: {host}\r\nOrigin: http://{host}\r\nAccept: text/vnd.turbo-stream.html\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
    let stream_response = send_raw_request(server.port, &stream_request);
    assert!(
        stream_response.starts_with("HTTP/1.1 200 OK\r\n"),
        "{stream_response}"
    );
    assert!(
        stream_response
            .to_ascii_lowercase()
            .contains("content-type: text/vnd.turbo-stream.html; charset=utf-8\r\n"),
        "{stream_response}"
    );
    assert_eq!(
        response_body(&stream_response),
        "<turbo-stream action=\"append\" target=\"todos\"><template>Tea &amp; toast &lt;/template&gt;&lt;script&gt;alert(1)&lt;/script&gt;</template></turbo-stream>"
    );

    // A valid Turbo response must not become a bypass for origin checks.
    for request in [
        format!(
            "POST /todos HTTP/1.1\r\nHost: {host}\r\nAccept: text/vnd.turbo-stream.html\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        ),
        format!(
            "POST /todos HTTP/1.1\r\nHost: {host}\r\nOrigin: https://attacker.example\r\nAccept: text/vnd.turbo-stream.html\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        ),
        format!(
            "POST /todos HTTP/1.1\r\nHost: {host}\r\nOrigin: http://{host}\r\nSec-Fetch-Site: cross-site\r\nAccept: text/vnd.turbo-stream.html\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        ),
    ] {
        let forbidden = send_raw_request(server.port, &request);
        assert!(
            forbidden.starts_with("HTTP/1.1 403 Forbidden\r\n"),
            "{forbidden}"
        );
        assert!(!forbidden.contains("<turbo-stream"), "{forbidden}");
        assert!(!forbidden.contains("text/vnd.turbo-stream.html"), "{forbidden}");
    }

    // A nominal session cookie is not a CSRF bypass: an absent/unknown
    // synchronizer token must be rejected before Turbo handler execution.
    let fake_session = "a".repeat(64);
    let forged_session_request = format!(
        "POST /todos HTTP/1.1\r\nHost: {host}\r\nOrigin: http://{host}\r\nCookie: nulang_session={fake_session}\r\nAccept: text/vnd.turbo-stream.html\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
    let denied_session = send_raw_request(server.port, &forged_session_request);
    assert!(
        denied_session.starts_with("HTTP/1.1 403 Forbidden\r\n"),
        "{denied_session}"
    );
    assert!(!denied_session.contains("<turbo-stream"));

    let html_response = send_raw_request(
        server.port,
        "GET /legacy HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    assert!(
        html_response
            .to_ascii_lowercase()
            .contains("content-type: text/html; charset=utf-8\r\n"),
        "{html_response}"
    );
    assert!(
        response_body(&html_response).starts_with("legacy"),
        "{html_response}"
    );
}

#[test]
fn turbo_stdlib_helpers_typecheck_with_import_resolution() {
    use std::collections::HashSet;
    use std::path::Path;
    let source_path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("conformance/behavior/web_19_turbo_stream.nula");
    let source = std::fs::read_to_string(&source_path).expect("read Turbo conformance fixture");
    let tokens = Lexer::new(&source).lex().expect("lex Turbo fixture");
    let mut ast = Parser::new(tokens)
        .parse_module()
        .expect("parse Turbo fixture");
    let mut visited = HashSet::new();
    nulang::resolver::resolve_imports(&mut ast, &source_path, &mut visited)
        .expect("resolve stdlib web turbo imports");
    TypeChecker::new()
        .check_module(&ast)
        .expect("typecheck imported Turbo helpers");
}
