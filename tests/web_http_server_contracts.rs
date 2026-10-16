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
fn static_asset_etags_support_revalidation_over_real_http() {
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct StaticRoot(PathBuf);
    impl Drop for StaticRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let root = StaticRoot(std::env::temp_dir().join(format!(
        "nulang-web-etag-{}-{nonce}",
        std::process::id()
    )));
    std::fs::create_dir_all(&root.0).unwrap();
    let asset = root.0.join("app.js");
    std::fs::write(&asset, "first").unwrap();

    let server = WebDevServer::bind_runtime(0, Some(root.0.clone()), None, vec![])
        .expect("bind static WebDevServer");
    let get = "GET /app.js HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
    let initial = send_raw_request(server.port, get);
    assert!(initial.starts_with("HTTP/1.1 200 OK\r\n"), "{initial}");
    assert_eq!(response_body(&initial), "first");
    let etag = response_header(&initial, "etag").expect("strong ETag on static asset");
    assert!(etag.starts_with('"') && etag.ends_with('"'), "{etag}");
    assert_eq!(response_header(&initial, "cache-control"), Some("no-cache"));

    let revalidated = send_raw_request(
        server.port,
        &format!("GET /app.js HTTP/1.1\r\nHost: localhost\r\nIf-None-Match: {etag}\r\n\r\n"),
    );
    assert!(revalidated.starts_with("HTTP/1.1 304 Not Modified\r\n"), "{revalidated}");
    assert_eq!(response_body(&revalidated), "");
    assert_eq!(response_header(&revalidated, "etag"), Some(etag));
    assert_eq!(response_header(&revalidated, "content-length"), Some("0"));

    let weak_and_list = send_raw_request(
        server.port,
        &format!(
            "GET /app.js HTTP/1.1\r\nHost: localhost\r\nIf-None-Match: \"not-this\", W/{etag}\r\n\r\n"
        ),
    );
    assert!(weak_and_list.starts_with("HTTP/1.1 304 Not Modified\r\n"), "{weak_and_list}");

    let non_matching = send_raw_request(
        server.port,
        "GET /app.js HTTP/1.1\r\nHost: localhost\r\nIf-None-Match: \"wrong\"\r\n\r\n",
    );
    assert!(non_matching.starts_with("HTTP/1.1 200 OK\r\n"), "{non_matching}");
    assert_eq!(response_body(&non_matching), "first");

    // Same length, new bytes: a size-only validator would incorrectly return 304.
    std::fs::write(&asset, "other").unwrap();
    let updated = send_raw_request(
        server.port,
        &format!("GET /app.js HTTP/1.1\r\nHost: localhost\r\nIf-None-Match: {etag}\r\n\r\n"),
    );
    assert!(updated.starts_with("HTTP/1.1 200 OK\r\n"), "{updated}");
    assert_eq!(response_body(&updated), "other");
    assert_ne!(response_header(&updated, "etag"), Some(etag));

    // Conditional GET is a static-asset optimization, not a mutation bypass.
    let post = send_raw_request(
        server.port,
        "POST /app.js HTTP/1.1\r\nHost: localhost\r\nIf-None-Match: *\r\nContent-Length: 0\r\n\r\n",
    );
    assert!(!post.starts_with("HTTP/1.1 304"), "{post}");
}

fn response_header<'a>(response: &'a str, key: &str) -> Option<&'a str> {
    response
        .split_once("\r\n\r\n")?
        .0
        .split("\r\n")
        .skip(1)
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case(key))
        .map(|(_, value)| value.trim())
}
