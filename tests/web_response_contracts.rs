use nulang::bytecode::CodeModule;
use nulang::lexer::Lexer;
use nulang::parser::Parser;
use nulang::runtime::{HttpMethod, WebDevServer, WebRoute};
use nulang::typechecker::TypeChecker;
use nulang::web::contracts::{compile_module_contracts, ContractCompilation, RouteContract};
use nulang::web::openapi::generate_openapi;
use nulang::web::response::{response_contract, ResponseBodyKind};
use nulang::web::runtime_bindings::{compile_runtime_route_plan, RuntimeWebRoute};
use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream};
use std::time::Duration;

fn parse(source: &str) -> nulang::ast::AstModule {
    let tokens = Lexer::new(source).lex().expect("lex source");
    Parser::new(tokens).parse_module().expect("parse source")
}

fn compile_handler(source: &str, handler_name: &str) -> (CodeModule, usize) {
    let ast = parse(source);
    let mut type_checker = TypeChecker::new();
    type_checker
        .check_module(&ast)
        .expect("typecheck handler source");
    let hir = nulang::hir_lower::lower_module(&ast, &type_checker.inferred_decl_types);
    let mut mir = nulang::mir_lower::lower_module(&hir).expect("lower handler MIR");
    let module =
        nulang::mir_codegen::compile_mir(&mut mir, "web-response-test").expect("compile bytecode");
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

fn route_with_response(response_type: &str) -> RouteContract {
    RouteContract {
        method: "GET".to_string(),
        path: "/value".to_string(),
        handler: Some("value".to_string()),
        params: Vec::new(),
        handler_params: Vec::new(),
        response_type: Some(response_type.to_string()),
        error_type: None,
        effects: Vec::new(),
        reference_capability: None,
        placement: Some("server".to_string()),
    }
}

fn json_runtime_route() -> RuntimeWebRoute {
    let source = r#"
type alias Json[T] = String
fn endpoint() -> Json[String] { "{\"ok\":true}" }
"#;
    let (module, function_index) = compile_handler(source, "endpoint");
    let contract_source = r#"
type alias Json[T] = String
fn endpoint() -> Json[String] { "{\"ok\":true}" }
fn web_main() { perform Web.route("GET", "/json", endpoint) }
"#;
    let contracts = compile_module_contracts(&parse(contract_source));
    assert!(
        contracts.diagnostics.is_empty(),
        "{:?}",
        contracts.diagnostics
    );
    assert_eq!(
        contracts.routes[0].response_type.as_deref(),
        Some("Json[String]")
    );

    let plan =
        compile_runtime_route_plan(&contracts.routes[0]).expect("compile response-aware plan");
    assert_eq!(
        plan.response.as_ref().map(|response| response.kind),
        Some(ResponseBodyKind::Json)
    );

    RuntimeWebRoute {
        route: WebRoute {
            method: HttpMethod::Get,
            path: "/json".to_string(),
            handler_module: module,
            handler_func_idx: function_index,
        },
        plan: Some(plan),
    }
}

fn send_raw_request(port: u16, request: &str) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect server");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set read timeout");
    stream.write_all(request.as_bytes()).expect("write request");
    stream.shutdown(Shutdown::Write).expect("finish request");
    let mut response = Vec::new();
    stream.read_to_end(&mut response).expect("read response");
    String::from_utf8(response).expect("response is utf-8")
}

fn response_body(response: &str) -> &str {
    response
        .split_once("\r\n\r\n")
        .expect("HTTP separator")
        .1
}

#[test]
fn response_contract_is_explicit_and_backwards_compatible() {
    let json = response_contract(Some("Json[User]")).unwrap();
    assert_eq!(json.kind, ResponseBodyKind::Json);
    assert_eq!(json.media_type, "application/json");
    assert_eq!(json.payload_type.as_deref(), Some("User"));
    assert!(!json.inject_client_runtime);
    assert!(response_contract(Some("String")).is_none());
}

#[test]
fn contract_extraction_preserves_json_wrapper() {
    let contracts = compile_module_contracts(&parse(
        r#"
type alias Json[T] = String
fn value() -> Json[String] { "{}" }
fn web_main() { perform Web.route("GET", "/value", value) }
"#,
    ));
    assert!(contracts.diagnostics.is_empty());
    assert_eq!(
        contracts.routes[0].response_type.as_deref(),
        Some("Json[String]")
    );
}

#[test]
fn openapi_uses_semantic_response_media() {
    let document = generate_openapi(
        &ContractCompilation {
            routes: vec![route_with_response("Json[User]")],
            diagnostics: Vec::new(),
        },
        "Response API",
        "1.0.0",
    );
    let success = &document["paths"]["/value"]["get"]["responses"]["200"];
    assert_eq!(success["x-nulang-response-type"], "Json[User]");
    assert_eq!(
        success["content"]["application/json"]["schema"]["x-nulang-type"],
        "User"
    );

    let html = generate_openapi(
        &ContractCompilation {
            routes: vec![route_with_response("Html")],
            diagnostics: Vec::new(),
        },
        "Response API",
        "1.0.0",
    );
    assert_eq!(
        html["paths"]["/value"]["get"]["responses"]["200"]["content"]["text/html"]["schema"]["type"],
        "string"
    );
}

#[test]
fn json_response_uses_json_content_type_without_client_runtime_injection() {
    let server = WebDevServer::bind_runtime(0, None, None, vec![json_runtime_route()])
        .expect("bind response-aware WebDevServer");
    let response = send_raw_request(
        server.port,
        "GET /json HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    assert!(
        response
            .to_ascii_lowercase()
            .contains("content-type: application/json\r\n"),
        "{response}"
    );
    assert_eq!(response_body(&response), "{\"ok\":true}");
    assert!(!response.contains("app.client.js"), "{response}");
}
