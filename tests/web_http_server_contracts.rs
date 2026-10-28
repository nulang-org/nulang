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

#[test]
fn live_web_dev_server_rejects_rebinding_before_routes_and_sse() {
    let server =
        WebDevServer::bind(0, None, None, vec![legacy_route()]).expect("bind WebDevServer");
    let forbidden = format!(
        "GET /legacy HTTP/1.1\r\nHost: attacker.example:{}\r\nConnection: close\r\n\r\n",
        server.port
    );
    let denied = send_raw_request(server.port, &forbidden);
    assert!(denied.starts_with("HTTP/1.1 403 Forbidden"), "{denied}");
    assert!(!denied.contains("legacy"));

    let sse = format!(
        "GET /__nulang/sse/private HTTP/1.1\r\nHost: attacker.example:{}\r\nConnection: close\r\n\r\n",
        server.port
    );
    let denied_sse = send_raw_request(server.port, &sse);
    assert!(
        denied_sse.starts_with("HTTP/1.1 403 Forbidden"),
        "{denied_sse}"
    );

    let trusted = send_raw_request(
        server.port,
        &format!(
            "GET /legacy HTTP/1.1\r\nHost: localhost:{}\r\nConnection: close\r\n\r\n",
            server.port
        ),
    );
    assert!(trusted.starts_with("HTTP/1.1 200 OK"), "{trusted}");
    assert!(response_body(&trusted).contains("legacy"));
}

#[test]
fn live_web_dev_server_rejects_unsafe_provenance_before_dispatch() {
    let server = WebDevServer::bind(0, None, None, vec![]).expect("bind WebDevServer");
    let host = format!("localhost:{}", server.port);
    for raw in [
        format!(
            "POST /mutation HTTP/1.1\r\nHost: {host}\r\nContent-Length: 0\r\n\r\n"
        ),
        format!(
            "POST /mutation HTTP/1.1\r\nHost: {host}\r\nOrigin: https://attacker.example\r\nContent-Length: 0\r\n\r\n"
        ),
        format!(
            "POST /mutation HTTP/1.1\r\nHost: {host}\r\nOrigin: http://{host}\r\nSec-Fetch-Site: cross-site\r\nContent-Length: 0\r\n\r\n"
        ),
        format!(
            "POST /mutation HTTP/1.1\r\nHost: {host}\r\nOrigin: http://{host}\r\nOrigin: https://attacker.example\r\nContent-Length: 0\r\n\r\n"
        ),
    ] {
        let denied = send_raw_request(server.port, &raw);
        assert!(denied.starts_with("HTTP/1.1 403 Forbidden"), "{denied}");
        assert!(!denied.contains("private_server_slot"));
    }

    let trusted = send_raw_request(
        server.port,
        &format!(
            "POST /mutation HTTP/1.1\r\nHost: {host}\r\nOrigin: http://{host}\r\nSec-Fetch-Site: same-origin\r\nContent-Length: 0\r\n\r\n"
        ),
    );
    assert!(trusted.starts_with("HTTP/1.1 404 Not Found"), "{trusted}");
}
