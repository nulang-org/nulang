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

#[test]
fn head_responses_for_registered_routes_retain_headers_but_omit_body() {
    let (module, function_index) = compile_handler("fn head() -> String { \"legacy\" }", "head");
    let server = WebDevServer::bind(
        0,
        None,
        None,
        vec![WebRoute {
            method: HttpMethod::Head,
            path: "/head".to_string(),
            handler_module: module,
            handler_func_idx: function_index,
        }],
    )
    .expect("bind HEAD route");

    let response = send_raw_request(
        server.port,
        "HEAD /head HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    assert!(response.contains("Content-Length: 6\r\n"), "{response}");
    assert_eq!(response_body(&response), "", "HEAD must not send a body");
}

#[test]
fn head_responses_for_static_assets_retain_length_but_omit_body() {
    struct TempDir(std::path::PathBuf);
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system time")
        .as_nanos();
    let directory =
        TempDir(std::env::temp_dir().join(format!("nulang-head-{}-{nonce}", std::process::id())));
    std::fs::create_dir_all(&directory.0).expect("create public directory");
    std::fs::write(directory.0.join("head.txt"), b"static-content").expect("write test asset");
    let server = WebDevServer::bind(0, Some(directory.0.clone()), None, Vec::new())
        .expect("bind static server");

    let get = send_raw_request(
        server.port,
        "GET /head.txt HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    let head = send_raw_request(
        server.port,
        "HEAD /head.txt HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    assert!(head.starts_with("HTTP/1.1 200 OK\r\n"), "{head}");
    assert!(head.contains("Content-Length: 14\r\n"), "{head}");
    assert_eq!(response_body(&get), "static-content");
    assert_eq!(response_body(&head), "", "HEAD must not send a body");
}

#[test]
fn head_requests_do_not_open_sse_streams() {
    let server = WebDevServer::bind(0, None, None, Vec::new()).expect("bind SSE server");
    let response = send_raw_request(
        server.port,
        "HEAD /__nulang/sse/room HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    assert!(
        response.starts_with("HTTP/1.1 405 Method Not Allowed\r\n"),
        "{response}"
    );
    assert!(response.contains("Allow: GET\r\n"), "{response}");
    assert_eq!(response_body(&response), "");
}

#[test]
fn post_requests_do_not_open_sse_streams() {
    let server = WebDevServer::bind(0, None, None, Vec::new()).expect("bind SSE server");
    let response = send_raw_request(
        server.port,
        "POST /__nulang/sse/room HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    );
    assert!(
        response.starts_with("HTTP/1.1 405 Method Not Allowed\r\n"),
        "{response}"
    );
    assert!(response.contains("Allow: GET\r\n"), "{response}");
    assert_eq!(response_body(&response), "Method not allowed");
}
