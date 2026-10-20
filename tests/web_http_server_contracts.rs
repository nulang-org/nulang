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


// A redirect is an HTTP response, not an HTML meta-refresh. Keep these as
// network-level tests so we verify the status and headers written to the wire.
fn redirect_route(method: HttpMethod, target: &str) -> WebRoute {
    let source = format!("fn bounce() {{ perform Web.redirect({target:?}) }}");
    let (module, function_index) = compile_handler(&source, "bounce");
    WebRoute {
        method,
        path: "/redirect".to_string(),
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

#[test]
fn web_redirect_returns_real_http_location_for_get() {
    let server = WebDevServer::bind(
        0,
        None,
        None,
        vec![redirect_route(HttpMethod::Get, "/dashboard?tab=recent")],
    )
    .expect("bind redirect route");

    let response = send_raw_request(
        server.port,
        "GET /redirect HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );

    assert!(response.starts_with("HTTP/1.1 302 Found\r\n"), "{response}");
    assert!(response.contains("Location: /dashboard?tab=recent\r\n"), "{response}");
    assert_eq!(response_body(&response), "");
}

#[test]
fn web_redirect_after_post_uses_see_other() {
    let server = WebDevServer::bind(
        0,
        None,
        None,
        vec![redirect_route(HttpMethod::Post, "/complete")],
    )
    .expect("bind redirect route");

    let response = send_raw_request(
        server.port,
        "POST /redirect HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    );

    assert!(response.starts_with("HTTP/1.1 303 See Other\r\n"), "{response}");
    assert!(response.contains("Location: /complete\r\n"), "{response}");
}

#[test]
fn web_redirect_rejects_external_and_header_injection_targets() {
    for unsafe_target in [
        "//outside.example/login",
        "https://outside.example/",
        "/safe\r\nX-Injected: yes",
    ] {
        let server = WebDevServer::bind(
            0,
            None,
            None,
            vec![redirect_route(HttpMethod::Get, unsafe_target)],
        )
        .expect("bind redirect route");

        let response = send_raw_request(
            server.port,
            "GET /redirect HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        );

        assert!(
            response.starts_with("HTTP/1.1 400 Bad Request\r\n"),
            "target {unsafe_target:?}: {response}"
        );
        assert!(!response.contains("Location:"), "{response}");
        assert!(!response.contains("X-Injected:"), "{response}");
    }
}


fn explicit_response_route(
    method: HttpMethod,
    source: &str,
    handler_name: &str,
) -> WebRoute {
    let (module, function_index) = compile_handler(source, handler_name);
    WebRoute {
        method,
        path: "/respond".to_string(),
        handler_module: module,
        handler_func_idx: function_index,
    }
}

#[test]
fn web_json_response_sets_status_media_type_and_preserves_json_bytes() {
    let source = r#"fn api() { perform Web.json_response(201, "{\"ok\":true,\"count\":2}") }"#;
    let server = WebDevServer::bind(
        0,
        None,
        None,
        vec![explicit_response_route(HttpMethod::Get, source, "api")],
    )
    .expect("bind JSON response route");

    let response = send_raw_request(
        server.port,
        "GET /respond HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    assert!(response.starts_with("HTTP/1.1 201 Created\r\n"), "{response}");
    assert!(
        response.contains("Content-Type: application/json; charset=utf-8\r\n"),
        "{response}"
    );
    assert_eq!(response_body(&response), r#"{"ok":true,"count":2}"#);
    assert!(!response.contains("nulang-client"), "{response}");
}

#[test]
fn web_text_response_uses_plain_text_without_html_encoding_or_script_injection() {
    let source = r#"fn api() { perform Web.text_response(422, "invalid <input> & retry") }"#;
    let server = WebDevServer::bind(
        0,
        None,
        None,
        vec![explicit_response_route(HttpMethod::Get, source, "api")],
    )
    .expect("bind text response route");

    let response = send_raw_request(
        server.port,
        "GET /respond HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    assert!(
        response.starts_with("HTTP/1.1 422 Unprocessable Entity\r\n"),
        "{response}"
    );
    assert!(
        response.contains("Content-Type: text/plain; charset=utf-8\r\n"),
        "{response}"
    );
    assert_eq!(response_body(&response), "invalid <input> & retry");
}

#[test]
fn web_json_response_rejects_invalid_json_without_returning_partial_success() {
    let source = r#"fn api() { perform Web.json_response(200, "not-json") }"#;
    let server = WebDevServer::bind(
        0,
        None,
        None,
        vec![explicit_response_route(HttpMethod::Get, source, "api")],
    )
    .expect("bind invalid JSON response route");

    let response = send_raw_request(
        server.port,
        "GET /respond HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    assert!(
        response.starts_with("HTTP/1.1 500 Internal Server Error\r\n"),
        "{response}"
    );
    assert_eq!(response_body(&response), "Invalid application response");
    assert!(!response.contains("not-json"), "{response}");
}

#[test]
fn web_response_rejects_statuses_that_cannot_have_this_body() {
    for source in [
        r#"fn api() { perform Web.text_response(204, "has a body") }"#,
        r#"fn api() { perform Web.text_response(302, "missing location") }"#,
        r#"fn api() { perform Web.json_response(700, "{}") }"#,
    ] {
        let server = WebDevServer::bind(
            0,
            None,
            None,
            vec![explicit_response_route(HttpMethod::Get, source, "api")],
        )
        .expect("bind invalid HTTP status response route");

        let response = send_raw_request(
            server.port,
            "GET /respond HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        );
        assert!(
            response.starts_with("HTTP/1.1 500 Internal Server Error\r\n"),
            "source {source}: {response}"
        );
        assert!(!response.contains("Location:"), "{response}");
    }
}
