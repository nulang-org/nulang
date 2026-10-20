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

fn typed_string_route() -> RuntimeWebRoute {
    let source = "fn text_endpoint() -> String { \"plain text\" }";
    let (module, function_index) = compile_handler(source, "text_endpoint");

    let contract_source = r#"
fn text_endpoint() -> String { "plain text" }

fn web_main() {
    perform Web.route("GET", "/text", text_endpoint)
}
"#;
    let tokens = Lexer::new(contract_source).lex().expect("lex text route");
    let ast = Parser::new(tokens)
        .parse_module()
        .expect("parse text route");
    let contracts = compile_module_contracts(&ast);
    assert!(contracts.diagnostics.is_empty(), "{:?}", contracts.diagnostics);
    let plan = compile_runtime_route_plan(&contracts.routes[0])
        .expect("compile text response route");
    assert_eq!(plan.response_type.as_deref(), Some("String"));
    RuntimeWebRoute {
        route: WebRoute {
            method: HttpMethod::Get,
            path: "/text".to_string(),
            handler_module: module,
            handler_func_idx: function_index,
        },
        plan: Some(plan),
    }
}

#[test]
fn compiled_string_response_is_plain_text_over_http() {
    let server = WebDevServer::bind_runtime(0, None, None, vec![typed_string_route()])
        .expect("bind text WebDevServer");
    let response = send_raw_request(
        server.port,
        "GET /text HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    assert!(
        response.contains("\r\nContent-Type: text/plain; charset=utf-8\r\n"),
        "{response}"
    );
    assert_eq!(response_body(&response), "plain text");
}

fn explicit_response_route(
    path: &str,
    method: HttpMethod,
    effect: &str,
    status: u16,
    body_source: &str,
) -> WebRoute {
    let source = format!("fn handler() {{ perform Web.{effect}({status}, {body_source}) }}");
    let (module, function_index) = compile_handler(&source, "handler");
    WebRoute {
        method,
        path: path.to_string(),
        handler_module: module,
        handler_func_idx: function_index,
    }
}

#[test]
fn explicit_json_response_uses_201_and_canonical_json_over_http() {
    let server = WebDevServer::bind(
        0,
        None,
        None,
        vec![explicit_response_route(
            "/created",
            HttpMethod::Post,
            "respond_json",
            201,
            r#""{\"ok\": true, \"id\": 7}""#,
        )],
    )
    .expect("bind explicit JSON WebDevServer");
    let response = send_raw_request(
        server.port,
        "POST /created HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    );
    assert!(response.starts_with("HTTP/1.1 201 Created\r\n"), "{response}");
    assert!(
        response.contains("\r\nContent-Type: application/json\r\n"),
        "{response}"
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(response_body(&response)).unwrap(),
        serde_json::json!({"ok": true, "id": 7})
    );
    assert!(!response_body(&response).contains("app.client.js"));
}

#[test]
fn explicit_text_response_can_return_non_default_status() {
    let server = WebDevServer::bind(
        0,
        None,
        None,
        vec![explicit_response_route(
            "/invalid",
            HttpMethod::Get,
            "respond_text",
            422,
            r#""validation failed""#,
        )],
    )
    .expect("bind explicit text WebDevServer");
    let response = send_raw_request(
        server.port,
        "GET /invalid HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    assert!(
        response.starts_with("HTTP/1.1 422 Unprocessable Entity\r\n"),
        "{response}"
    );
    assert!(
        response.contains("\r\nContent-Type: text/plain; charset=utf-8\r\n"),
        "{response}"
    );
    assert_eq!(response_body(&response), "validation failed");
}

#[test]
fn invalid_explicit_json_and_unsupported_status_fail_closed() {
    let server = WebDevServer::bind(
        0,
        None,
        None,
        vec![
            explicit_response_route(
                "/invalid-json",
                HttpMethod::Get,
                "respond_json",
                200,
                r#""{not valid json}""#,
            ),
            explicit_response_route(
                "/invalid-status",
                HttpMethod::Get,
                "respond_text",
                601,
                r#""private error details""#,
            ),
        ],
    )
    .expect("bind invalid response routes");
    for (path, rejected_body) in [
        ("/invalid-json", "{not valid json}"),
        ("/invalid-status", "private error details"),
    ] {
        let response = send_raw_request(
            server.port,
            &format!(
                "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
            ),
        );
        assert!(
            response.starts_with("HTTP/1.1 500 Internal Server Error\r\n"),
            "{response}"
        );
        assert!(!response_body(&response).contains(rejected_body), "{response}");
        assert!(!response.contains("\r\nLocation:"), "{response}");
    }
}

#[test]
fn explicit_response_metadata_does_not_escape_its_request() {
    let server = WebDevServer::bind(
        0,
        None,
        None,
        vec![
            explicit_response_route(
                "/json",
                HttpMethod::Get,
                "respond_json",
                201,
                r#""{\"saved\":true}""#,
            ),
            legacy_route(),
        ],
    )
    .expect("bind response and legacy routes");
    let first = send_raw_request(
        server.port,
        "GET /json HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    assert!(first.starts_with("HTTP/1.1 201 Created\r\n"), "{first}");

    let second = send_raw_request(
        server.port,
        "GET /legacy HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    assert!(second.starts_with("HTTP/1.1 200 OK\r\n"), "{second}");
    assert!(
        second.contains("\r\nContent-Type: text/html; charset=utf-8\r\n"),
        "{second}"
    );
    assert!(response_body(&second).starts_with("legacy"), "{second}");
}
