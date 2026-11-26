use nulang::bytecode::CodeModule;
use nulang::lexer::Lexer;
use nulang::parser::Parser;
use nulang::runtime::{HttpMethod, WebDevServer, WebRoute};
use nulang::typechecker::TypeChecker;
use nulang::web::contracts::compile_module_contracts;
use nulang::web::dispatch::{compile_runtime_routes, compile_runtime_routes_from_contracts};
use nulang::web::openapi::generate_openapi;
use nulang::web::runtime_bindings::{compile_runtime_route_plan, RuntimeWebRoute};
use nulang::web::validation::compile_validated_contracts_from_tree;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream};
use std::path::PathBuf;
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

fn isolated_web_contract_test_dir(name: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT_ID: AtomicU64 = AtomicU64::new(0);
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "nulang-web-http-contract-{name}-{}-{id}",
        std::process::id()
    ));
    std::fs::create_dir(&path).expect("create isolated source directory");
    path
}

#[test]
fn unvalidated_ambiguous_contracts_must_not_attach_to_http_runtime() {
    let source = r#"
fn show_id(id: Int) -> Int { id }
fn show_user(user_id: Int) -> Int { user_id }
fn web_main() {
    perform Web.route("GET", "/items/{id: Int}", show_id)
    perform Web.route("GET", "/items/:user_id", show_user)
}
"#;
    let tokens = Lexer::new(source).lex().expect("lex conflicting contracts");
    let module = Parser::new(tokens)
        .parse_module()
        .expect("parse conflicting contracts");
    let contracts = compile_module_contracts(&module);
    assert!(
        contracts.diagnostics.is_empty(),
        "{:?}",
        contracts.diagnostics
    );
    assert_eq!(contracts.routes.len(), 2);

    let error = compile_runtime_routes_from_contracts(vec![typed_query_route().route], &contracts)
        .expect_err("runtime attachment must revalidate untrusted contract input");
    assert!(
        error
            .iter()
            .any(|diagnostic| diagnostic.contains("ambiguous route GET")),
        "{error:?}"
    );
}

#[test]
fn package_route_preflight_rejects_duplicates_before_http_bind_and_allows_valid_route() {
    let dir = isolated_web_contract_test_dir("duplicates");
    let source_file = dir.join("main.nula");
    let handler = r#"
fn endpoint(limit: Int from query) -> Int { limit }
fn web_main() {
    perform Web.route("GET", "/users", endpoint)
"#;
    std::fs::write(
        &source_file,
        format!("{handler}    perform Web.route(\"GET\", \"/users\", endpoint)\n}}\n"),
    )
    .expect("write conflicting package");

    // A real compiled VM handler must not be accepted for an ambiguous package.
    let error = compile_runtime_routes(vec![typed_query_route().route], &dir)
        .expect_err("duplicate package registration must block HTTP attachment");
    assert!(
        error
            .iter()
            .any(|diagnostic| diagnostic.contains("duplicate web route GET /users")),
        "{error:?}"
    );
    assert!(
        compile_validated_contracts_from_tree(&dir).is_err(),
        "invalid package must not be exported to OpenAPI or deployment IR"
    );

    std::fs::write(&source_file, format!("{handler}}}\n")).expect("write valid package");
    let contracts = compile_validated_contracts_from_tree(&dir).expect("validate route");
    let openapi = generate_openapi(&contracts, "Test API", "1.0");
    assert_eq!(openapi["paths"]["/users"]["get"]["operationId"], "endpoint");

    let routes = compile_runtime_routes(vec![typed_query_route().route], &dir)
        .expect("valid route must attach");
    assert_eq!(routes.len(), 1);
    let server = WebDevServer::bind_runtime(0, None, None, routes).expect("start listener");
    let response = send_raw_request(
        server.port,
        "GET /users?limit=25 HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    assert!(response_body(&response).starts_with("25"), "{response}");

    drop(server);
    std::fs::remove_dir_all(&dir).expect("remove isolated source directory");
}
