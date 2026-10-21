use nulang::bytecode::CodeModule;
use nulang::lexer::Lexer;
use nulang::parser::Parser;
use nulang::runtime::{HttpMethod, HttpRequest, WebDevServer, WebRoute};
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
fn injected_typed_request_uses_same_route_contracts() {
    let response = WebDevServer::inject(
        &[typed_query_route()],
        HttpRequest {
            method: HttpMethod::Get,
            path: "/users?limit=25".into(),
            headers: vec![("Host".into(), "localhost".into())],
            body: vec![],
        },
        None,
        None,
    );

    assert_eq!(response.status, 200);
    assert!(
        String::from_utf8(response.body).unwrap().starts_with("25"),
        "injected request must execute the compiled typed handler"
    );
    assert!(response.headers.iter().any(|(name, value)| {
        name.eq_ignore_ascii_case("X-Content-Type-Options") && value == "nosniff"
    }));
}

#[test]
fn injected_invalid_query_is_a_structured_client_error() {
    let response = WebDevServer::inject(
        &[typed_query_route()],
        HttpRequest {
            method: HttpMethod::Get,
            path: "/users?limit=many".into(),
            headers: vec![],
            body: vec![],
        },
        None,
        None,
    );

    assert_eq!(response.status, 400);
    assert!(response.headers.iter().any(|(name, value)| {
        name.eq_ignore_ascii_case("Content-Type") && value == "application/problem+json"
    }));
    let problem: serde_json::Value =
        serde_json::from_slice(&response.body).expect("valid problem JSON");
    assert_eq!(problem["code"], "invalid_request_input");
    assert_eq!(problem["source"], "query");
}

#[test]
fn injected_legacy_route_and_not_found_match_socket_dispatch() {
    let routes = vec![RuntimeWebRoute {
        route: legacy_route(),
        plan: None,
    }];
    let request = |path: &str| HttpRequest {
        method: HttpMethod::Get,
        path: path.to_string(),
        headers: vec![],
        body: vec![],
    };
    let legacy = WebDevServer::inject(&routes, request("/legacy"), None, None);
    assert_eq!(legacy.status, 200);
    assert!(String::from_utf8(legacy.body).unwrap().starts_with("legacy"));

    let missing = WebDevServer::inject(&routes, request("/absent"), None, None);
    assert_eq!(missing.status, 404);
    assert_eq!(missing.body, b"Not found");
}

#[test]
fn injected_streaming_endpoint_fails_explicitly_instead_of_hanging() {
    let response = WebDevServer::inject(
        &[],
        HttpRequest {
            method: HttpMethod::Get,
            path: "/__nulang/sse/events".into(),
            headers: vec![],
            body: vec![],
        },
        None,
        None,
    );
    assert_eq!(response.status, 501);
    assert_eq!(response.body, b"Streaming is not supported by request injection");
}
