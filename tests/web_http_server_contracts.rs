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

fn explicit_response_route(kind: &str, payload: &str, path: &str) -> RuntimeWebRoute {
    let op = match kind {
        "Json" => "json",
        "Text" => "plaintext",
        _ => panic!("unsupported response kind"),
    };
    let source = format!(
        r#"
opaque type Json = String
opaque type Text = String
fn endpoint() -> {kind} ! {{Web}} {{ perform Web.{op}({payload}) }}
fn web_main() {{ perform Web.route("GET", "{path}", endpoint) }}
"#
    );
    let (module, handler_func_idx) = compile_handler(&source, "endpoint");
    let ast = Parser::new(Lexer::new(&source).lex().unwrap())
        .parse_module()
        .unwrap();
    let contracts = compile_module_contracts(&ast);
    assert!(
        contracts.diagnostics.is_empty(),
        "{:?}",
        contracts.diagnostics
    );
    assert_eq!(
        contracts.routes[0].response_type.as_deref(),
        Some(kind),
        "compiler response type: {:?}",
        contracts.routes[0]
    );
    let plan = compile_runtime_route_plan(&contracts.routes[0]).unwrap();
    assert_ne!(
        plan.response_kind,
        nulang::web::response::ResponseKind::Html
    );
    RuntimeWebRoute {
        route: WebRoute {
            method: HttpMethod::Get,
            path: path.into(),
            handler_module: module,
            handler_func_idx,
        },
        plan: Some(plan),
    }
}

#[test]
fn explicit_json_response_has_json_media_type_without_html_injection() {
    let route = explicit_response_route("Json", r#""{\"ok\":true}""#, "/json");
    let server = WebDevServer::bind_runtime(0, None, None, vec![route]).unwrap();
    let response = send_raw_request(
        server.port,
        "GET /json HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    assert!(response.contains("Content-Type: application/json; charset=utf-8\r\n"));
    assert_eq!(response_body(&response), r#"{"ok":true}"#);
}

#[test]
fn explicit_plaintext_response_preserves_markup_without_script_injection() {
    let route = explicit_response_route("Text", r#""<p>hello</p>""#, "/text");
    let server = WebDevServer::bind_runtime(0, None, None, vec![route]).unwrap();
    let response = send_raw_request(
        server.port,
        "GET /text HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    assert!(response.contains("Content-Type: text/plain; charset=utf-8\r\n"));
    assert_eq!(response_body(&response), "<p>hello</p>");
}

#[test]
fn invalid_json_response_fails_closed_without_leaking_payload() {
    let route = explicit_response_route("Json", r#""not valid JSON""#, "/invalid-json");
    let server = WebDevServer::bind_runtime(0, None, None, vec![route]).unwrap();
    let response = send_raw_request(
        server.port,
        "GET /invalid-json HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    assert!(
        response.starts_with("HTTP/1.1 500 Internal Server Error\r\n"),
        "{response}"
    );
    assert_eq!(response_body(&response), "Internal server error");
}

#[test]
fn imported_nominal_json_response_compiles_and_serves_json() {
    // This test exercises the direct compiler/VM seam. Module-function imports
    // are linked by the package pipeline, not by TypeChecker::check_module.
    let source = r#"
import stdlib::web::types
fn endpoint() -> Json ! {Web} {
    perform Web.json("{\"ok\":true}")
}
fn web_main() { perform Web.route("GET", "/health", endpoint) }
"#;
    let (module, handler_func_idx) = compile_handler(source, "endpoint");
    let ast = Parser::new(Lexer::new(source).lex().unwrap())
        .parse_module()
        .unwrap();
    let contracts = compile_module_contracts(&ast);
    assert!(
        contracts.diagnostics.is_empty(),
        "{:?}",
        contracts.diagnostics
    );
    assert_eq!(contracts.routes[0].response_type.as_deref(), Some("Json"));
    let route = RuntimeWebRoute {
        route: WebRoute {
            method: HttpMethod::Get,
            path: "/health".into(),
            handler_module: module,
            handler_func_idx,
        },
        plan: Some(compile_runtime_route_plan(&contracts.routes[0]).unwrap()),
    };
    let server = WebDevServer::bind_runtime(0, None, None, vec![route]).unwrap();
    let response = send_raw_request(
        server.port,
        "GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    assert!(response.contains("Content-Type: application/json; charset=utf-8\r\n"));
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(response_body(&response)).unwrap()["ok"],
        true
    );
}

fn status_result_route(path: &str, kind: &str, expression: &str) -> RuntimeWebRoute {
    let source = format!(
        r#"
opaque type Created = String
opaque type NoContent = String
opaque type Problem = String
fn endpoint() -> {kind} ! {{Web}} {{ {expression} }}
fn web_main() {{ perform Web.route("GET", "{path}", endpoint) }}
"#
    );
    let (module, handler_func_idx) = compile_handler(&source, "endpoint");
    let ast = Parser::new(Lexer::new(&source).lex().unwrap())
        .parse_module()
        .unwrap();
    let contracts = compile_module_contracts(&ast);
    assert!(
        contracts.diagnostics.is_empty(),
        "{:?}",
        contracts.diagnostics
    );
    assert_eq!(contracts.routes[0].response_type.as_deref(), Some(kind));
    RuntimeWebRoute {
        route: WebRoute {
            method: HttpMethod::Get,
            path: path.into(),
            handler_module: module,
            handler_func_idx,
        },
        plan: Some(compile_runtime_route_plan(&contracts.routes[0]).unwrap()),
    }
}

#[test]
fn created_response_is_201_json_over_real_http() {
    let route = status_result_route(
        "/created",
        "Created",
        r#"perform Web.created("{\"id\":42}")"#,
    );
    let server = WebDevServer::bind_runtime(0, None, None, vec![route]).unwrap();
    let response = send_raw_request(
        server.port,
        "GET /created HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    assert!(
        response.starts_with("HTTP/1.1 201 Created\r\n"),
        "{response}"
    );
    assert!(
        response.contains("Content-Type: application/json; charset=utf-8\r\n"),
        "{response}"
    );
    assert_eq!(response_body(&response), r#"{"id":42}"#);
}

#[test]
fn no_content_response_is_204_with_no_body_or_media_type() {
    let route = status_result_route("/empty", "NoContent", "perform Web.no_content()");
    let server = WebDevServer::bind_runtime(0, None, None, vec![route]).unwrap();
    let response = send_raw_request(
        server.port,
        "GET /empty HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    assert!(
        response.starts_with("HTTP/1.1 204 No Content\r\n"),
        "{response}"
    );
    assert!(
        !response.to_ascii_lowercase().contains("content-type:"),
        "{response}"
    );
    assert!(response.contains("Content-Length: 0\r\n"), "{response}");
    assert_eq!(response_body(&response), "");
}

#[test]
fn problem_result_uses_document_status_and_problem_media_type() {
    let route = status_result_route(
        "/problem",
        "Problem",
        r#"perform Web.problem(409, "Conflict", "Resource exists")"#,
    );
    let server = WebDevServer::bind_runtime(0, None, None, vec![route]).unwrap();
    let response = send_raw_request(
        server.port,
        "GET /problem HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    assert!(
        response.starts_with("HTTP/1.1 409 Conflict\r\n"),
        "{response}"
    );
    assert!(
        response.contains("Content-Type: application/problem+json\r\n"),
        "{response}"
    );
    let body: serde_json::Value = serde_json::from_str(response_body(&response)).unwrap();
    assert_eq!(body["status"], 409);
    assert_eq!(body["title"], "Conflict");
    assert_eq!(body["detail"], "Resource exists");
}

#[test]
fn invalid_problem_status_fails_closed_instead_of_returning_success() {
    let route = status_result_route(
        "/invalid-problem",
        "Problem",
        r#"perform Web.problem(200, "OK", "Not an error")"#,
    );
    let server = WebDevServer::bind_runtime(0, None, None, vec![route]).unwrap();
    let response = send_raw_request(
        server.port,
        "GET /invalid-problem HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    assert!(
        response.starts_with("HTTP/1.1 500 Internal Server Error\r\n"),
        "{response}"
    );
    assert_eq!(response_body(&response), "Internal server error");
}
