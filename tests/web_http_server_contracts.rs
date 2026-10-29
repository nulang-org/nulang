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
opaque type InvalidForm = String
opaque type FormOutcome = String
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
fn invalid_form_returns_422_html_fragment_over_http() {
    let route = status_result_route(
        "/invalid-form",
        "InvalidForm",
        r#"perform Web.invalid_form(perform Web.raw("<form id='edit'><p>Required</p></form>"))"#,
    );
    let server = WebDevServer::bind_runtime(0, None, None, vec![route]).unwrap();
    let response = send_raw_request(
        server.port,
        "GET /invalid-form HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    assert!(
        response.starts_with("HTTP/1.1 422 Unprocessable Entity\r\n"),
        "{response}"
    );
    assert!(
        response.contains("Content-Type: text/html; charset=utf-8\r\n"),
        "{response}"
    );
    assert_eq!(
        response_body(&response),
        "<form id='edit'><p>Required</p></form>"
    );
}

#[test]
fn one_form_handler_returns_200_or_422_from_the_same_route() {
    let route = status_result_route(
        "/form-outcome",
        "FormOutcome",
        r#"if perform Web.header("X-Form-Valid") == "true" then {
            perform Web.form_ok(perform Web.raw("<section id='saved'>Saved</section>"))
        } else {
            perform Web.form_invalid(perform Web.raw("<form id='edit'><p>Required</p></form>"))
        }"#,
    );
    let server = WebDevServer::bind_runtime(0, None, None, vec![route]).unwrap();
    let success = send_raw_request(
        server.port,
        "GET /form-outcome HTTP/1.1\r\nHost: localhost\r\nX-Form-Valid: true\r\nConnection: close\r\n\r\n",
    );
    let invalid = send_raw_request(
        server.port,
        "GET /form-outcome HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    assert!(success.starts_with("HTTP/1.1 200 OK\r\n"), "{success}");
    assert!(
        invalid.starts_with("HTTP/1.1 422 Unprocessable Entity\r\n"),
        "{invalid}"
    );
    for result in [&success, &invalid] {
        assert!(
            result.contains("Content-Type: text/html; charset=utf-8\r\n"),
            "{result}"
        );
    }
    assert_eq!(
        response_body(&success),
        "<section id='saved'>Saved</section>"
    );
    assert_eq!(
        response_body(&invalid),
        "<form id='edit'><p>Required</p></form>"
    );
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

#[test]
fn htmx_response_header_round_trips_over_real_http() {
    let (module, function_index) = compile_handler(
        r#"fn endpoint() -> String ! {Web} {
    perform Web.set_header("HX-Trigger", "saved")
    "fragment"
}"#,
        "endpoint",
    );
    let server = WebDevServer::bind(
        0,
        None,
        None,
        vec![WebRoute {
            method: HttpMethod::Get,
            path: "/fragment".to_string(),
            handler_module: module,
            handler_func_idx: function_index,
        }],
    )
    .expect("bind WebDevServer");

    let response = send_raw_request(
        server.port,
        "GET /fragment HTTP/1.1\r\nHost: localhost\r\nHX-Request: true\r\nConnection: close\r\n\r\n",
    );
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    assert!(response.contains("\r\nHX-Trigger: saved\r\n"), "{response}");
    assert_eq!(response_body(&response), "fragment");
}

#[test]
fn htmx_response_header_can_vary_by_request_without_leaking_to_other_requests() {
    let (module, function_index) = compile_handler(
        r#"fn endpoint() -> String ! {Web} {
    let hx = perform Web.header("HX-Request")
    if hx == "true" then {
        perform Web.set_header("Vary", "HX-Request")
        perform Web.set_header("HX-Trigger", "updated")
        "fragment"
    } else {
        "full"
    }
}"#,
        "endpoint",
    );
    let server = WebDevServer::bind(
        0,
        None,
        None,
        vec![WebRoute {
            method: HttpMethod::Get,
            path: "/page".to_string(),
            handler_module: module,
            handler_func_idx: function_index,
        }],
    )
    .expect("bind WebDevServer");

    let fragment = send_raw_request(
        server.port,
        "GET /page HTTP/1.1\r\nHost: localhost\r\nHX-Request: true\r\nConnection: close\r\n\r\n",
    );
    assert!(
        fragment.contains("\r\nHX-Trigger: updated\r\n"),
        "{fragment}"
    );
    assert!(fragment.contains("\r\nVary: HX-Request\r\n"), "{fragment}");
    assert_eq!(response_body(&fragment), "fragment");

    let page = send_raw_request(
        server.port,
        "GET /page HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    assert!(!page.contains("HX-Trigger:"), "{page}");
    assert!(!page.contains("\r\nVary:"), "{page}");
    assert_eq!(response_body(&page), "full");
}

#[test]
fn response_headers_block_crlf_injection_and_server_owned_fields_over_real_http() {
    let (module, function_index) = compile_handler(
        r#"fn endpoint() -> String ! {Web} {
    perform Web.set_header("HX-Trigger", "ok\r\nX-Injected: true")
    perform Web.set_header("Content-Length", "1")
    perform Web.set_header("X-Frame-Options", "ALLOWALL")
    perform Web.set_header("X-App-Header", "safe")
    "safe"
}"#,
        "endpoint",
    );
    let server = WebDevServer::bind(
        0,
        None,
        None,
        vec![WebRoute {
            method: HttpMethod::Get,
            path: "/safe".to_string(),
            handler_module: module,
            handler_func_idx: function_index,
        }],
    )
    .expect("bind WebDevServer");

    let response = send_raw_request(
        server.port,
        "GET /safe HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    assert_eq!(response_body(&response), "safe");
    assert!(!response.contains("\r\nX-Injected: true\r\n"), "{response}");
    assert!(!response.contains("\r\nHX-Trigger:"), "{response}");
    assert!(
        response.contains("\r\nX-App-Header: safe\r\n"),
        "{response}"
    );
    assert!(response.contains("\r\nContent-Length: 4\r\n"), "{response}");
    assert_eq!(response.matches("\r\nContent-Length:").count(), 1);
    assert!(
        response.contains("\r\nX-Frame-Options: DENY\r\n"),
        "{response}"
    );
}

fn legacy_route_at(method: HttpMethod, path: &str, label: &str) -> WebRoute {
    let source = format!("fn page() -> String {{ \"{label}\" }}");
    let (handler_module, handler_func_idx) = compile_handler(&source, "page");
    WebRoute {
        method,
        path: path.to_string(),
        handler_module,
        handler_func_idx,
    }
}

#[test]
fn literal_routes_win_over_parameter_routes_regardless_of_registration_order() {
    for literal_first in [true, false] {
        let literal = legacy_route_at(HttpMethod::Get, "/users/me", "literal");
        let param = legacy_route_at(HttpMethod::Get, "/users/:id", "parameter");
        let routes = if literal_first {
            vec![literal, param]
        } else {
            vec![param, literal]
        };
        let server = WebDevServer::bind(0, None, None, routes).expect("bind routes");
        let response = send_raw_request(
            server.port,
            "GET /users/me HTTP/1.1\r\nHost: localhost\r\n\r\n",
        );
        assert_eq!(response_body(&response), "literal");
    }
}

#[test]
fn unsupported_method_returns_405_and_accurate_allow_header() {
    let server = WebDevServer::bind(
        0,
        None,
        None,
        vec![legacy_route_at(HttpMethod::Get, "/resource", "ok")],
    )
    .expect("bind route");
    let response = send_raw_request(
        server.port,
        "POST /resource HTTP/1.1\r\nHost: localhost\r\n\r\n",
    );
    assert!(
        response.starts_with("HTTP/1.1 405 Method Not Allowed\r\n"),
        "{response}"
    );
    assert!(
        response.contains("Allow: GET, HEAD, OPTIONS\r\n"),
        "{response}"
    );
}

#[test]
fn options_automatically_reports_allowed_methods() {
    let server = WebDevServer::bind(
        0,
        None,
        None,
        vec![legacy_route_at(HttpMethod::Get, "/resource", "ok")],
    )
    .expect("bind route");
    let response = send_raw_request(
        server.port,
        "OPTIONS /resource HTTP/1.1\r\nHost: localhost\r\n\r\n",
    );
    assert!(
        response.starts_with("HTTP/1.1 204 No Content\r\n"),
        "{response}"
    );
    assert!(
        response.contains("Allow: GET, HEAD, OPTIONS\r\n"),
        "{response}"
    );
    assert!(response_body(&response).is_empty(), "{response}");
}

#[test]
fn head_falls_back_to_get_and_sends_no_body_with_representation_length() {
    let server = WebDevServer::bind(
        0,
        None,
        None,
        vec![legacy_route_at(HttpMethod::Get, "/resource", "hello")],
    )
    .expect("bind route");
    let response = send_raw_request(
        server.port,
        "HEAD /resource HTTP/1.1\r\nHost: localhost\r\n\r\n",
    );
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    assert!(response.contains("Content-Length: 5\r\n"), "{response}");
    assert!(response_body(&response).is_empty(), "{response}");
}

#[test]
fn static_files_reject_post_and_serve_head_without_body() {
    let path = std::env::temp_dir().join(format!(
        "nulang-web-static-method-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&path).unwrap();
    std::fs::write(path.join("sample.txt"), b"contents").unwrap();
    let server = WebDevServer::bind(0, Some(path.clone()), None, vec![]).unwrap();

    let post = send_raw_request(
        server.port,
        "POST /sample.txt HTTP/1.1\r\nHost: localhost\r\n\r\n",
    );
    assert!(
        post.starts_with("HTTP/1.1 405 Method Not Allowed\r\n"),
        "{post}"
    );
    assert!(post.contains("Allow: GET, HEAD, OPTIONS\r\n"), "{post}");

    let head = send_raw_request(
        server.port,
        "HEAD /sample.txt HTTP/1.1\r\nHost: localhost\r\n\r\n",
    );
    assert!(head.starts_with("HTTP/1.1 200 OK\r\n"), "{head}");
    assert!(head.contains("Content-Length: 8\r\n"), "{head}");
    assert!(response_body(&head).is_empty(), "{head}");

    drop(server);
    std::fs::remove_dir_all(path).unwrap();
}

#[test]
fn sse_endpoint_does_not_stream_for_post() {
    let server = WebDevServer::bind(0, None, None, vec![]).unwrap();
    let response = send_raw_request(
        server.port,
        "POST /__nulang/sse/room HTTP/1.1\r\nHost: localhost\r\n\r\n",
    );
    assert!(
        response.starts_with("HTTP/1.1 405 Method Not Allowed\r\n"),
        "{response}"
    );
}

fn json_scalar_route() -> RuntimeWebRoute {
    let (module, function_index) =
        compile_handler("fn endpoint(value: Int) -> Int { value }", "endpoint");
    let source = r#"
fn endpoint(value: Int from json) -> Int { value }
fn web_main() { perform Web.route("POST", "/json", endpoint) }
"#;
    let ast = Parser::new(Lexer::new(source).lex().unwrap())
        .parse_module()
        .unwrap();
    let contract = compile_module_contracts(&ast);
    assert!(
        contract.diagnostics.is_empty(),
        "{:?}",
        contract.diagnostics
    );
    let plan = compile_runtime_route_plan(&contract.routes[0]).unwrap();
    RuntimeWebRoute {
        route: WebRoute {
            method: HttpMethod::Post,
            path: "/json".into(),
            handler_module: module,
            handler_func_idx: function_index,
        },
        plan: Some(plan),
    }
}

#[test]
fn typed_json_request_decodes_over_real_http() {
    let server = WebDevServer::bind_runtime(0, None, None, vec![json_scalar_route()]).unwrap();
    let response = send_raw_request(
        server.port,
        "POST /json HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n42",
    );
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    assert_eq!(response_body(&response), "42");
}

#[test]
fn typed_json_request_rejects_wrong_media_type_and_malformed_json() {
    let server = WebDevServer::bind_runtime(0, None, None, vec![json_scalar_route()]).unwrap();
    let wrong_type = send_raw_request(
        server.port,
        "POST /json HTTP/1.1\r\nHost: localhost\r\nContent-Type: text/plain\r\nContent-Length: 2\r\n\r\n42",
    );
    assert!(wrong_type.starts_with("HTTP/1.1 415"), "{wrong_type}");
    let invalid = send_raw_request(
        server.port,
        "POST /json HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: 8\r\n\r\nnot-json",
    );
    assert!(invalid.starts_with("HTTP/1.1 400"), "{invalid}");
}

fn json_response_route(json: &str) -> RuntimeWebRoute {
    let expression = format!("fn endpoint() -> String {{ {:?} }}", json);
    let (module, function_index) = compile_handler(&expression, "endpoint");
    let mut contract = nulang::web::contracts::RouteContract::default();
    contract.method = "GET".into();
    contract.path = "/response".into();
    contract.handler = Some("endpoint".into());
    contract.response_type = Some("Json".into());
    RuntimeWebRoute {
        route: WebRoute {
            method: HttpMethod::Get,
            path: "/response".into(),
            handler_module: module,
            handler_func_idx: function_index,
        },
        plan: Some(compile_runtime_route_plan(&contract).unwrap()),
    }
}

#[test]
fn json_response_contract_controls_content_type_and_body_over_http() {
    let server =
        WebDevServer::bind_runtime(0, None, None, vec![json_response_route(r#"{"ok":true}"#)])
            .unwrap();
    let response = send_raw_request(
        server.port,
        "GET /response HTTP/1.1\r\nHost: localhost\r\n\r\n",
    );
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    assert!(
        response.contains("Content-Type: application/json\r\n"),
        "{response}"
    );
    assert_eq!(response_body(&response), r#"{"ok":true}"#);
}

#[test]
fn invalid_json_handler_response_is_not_served_as_success() {
    let server =
        WebDevServer::bind_runtime(0, None, None, vec![json_response_route("<not-json>")]).unwrap();
    let response = send_raw_request(
        server.port,
        "GET /response HTTP/1.1\r\nHost: localhost\r\n\r\n",
    );
    assert!(
        response.starts_with("HTTP/1.1 500 Internal Server Error\r\n"),
        "{response}"
    );
    assert!(!response.contains("<not-json>"));
}

#[test]
fn json_request_with_invalid_utf8_is_rejected_before_lossy_decode() {
    let server = WebDevServer::bind_runtime(0, None, None, vec![json_scalar_route()]).unwrap();
    let mut stream = TcpStream::connect(("127.0.0.1", server.port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream.write_all(b"POST /json HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: 3\r\n\r\n\x22\xff\x22").unwrap();
    stream.shutdown(Shutdown::Write).unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).unwrap();
    let response = String::from_utf8(response).unwrap();
    assert!(
        response.starts_with("HTTP/1.1 400 Bad Request\r\n"),
        "{response}"
    );
    assert!(response.contains("application/problem+json"), "{response}");
}

#[test]
fn json_response_effect_and_route_registration_are_typechecked() {
    let source = r#"
opaque type Json = String
fn endpoint() -> Json { perform Web.json("{\"ok\":true}") }
fn register() { perform Web.route_json("GET", "/api", endpoint) }
"#;
    let (mut module, func_idx) = compile_handler(source, "register");
    let ast = Parser::new(Lexer::new(source).lex().unwrap())
        .parse_module()
        .unwrap();
    let contracts = compile_module_contracts(&ast);
    assert!(
        contracts.diagnostics.is_empty(),
        "{:?}",
        contracts.diagnostics
    );
    assert_eq!(contracts.routes.len(), 1);
    assert_eq!(contracts.routes[0].response_type.as_deref(), Some("Json"));

    // Execute the compiled route-registration function through the real VM
    // instead of constructing a route manually. The client should then see
    // the declared Json response media type from compiler route metadata.
    use nulang::bytecode::{Instruction, OpCode};
    use nulang::vm::VM;
    let offset = module.instructions.len();
    module.emit(Instruction::new3(
        OpCode::Closure,
        ((func_idx >> 8) & 0xff) as u8,
        (func_idx & 0xff) as u8,
        0,
    ));
    module.emit(Instruction::new3(OpCode::ClosureCall, 0, 0, 0));
    module.emit(Instruction::new0(OpCode::Ret));
    let mut vm = VM::new();
    vm.load_module(module);
    vm.run_from(0, offset).expect("execute JSON registration");
    let routes = vm.take_web_routes();
    assert_eq!(routes.len(), 1);
    let attachment = nulang::web::runtime_bindings::attach_runtime_route_plans(routes, &contracts);
    assert!(
        attachment.diagnostics.is_empty(),
        "{:?}",
        attachment.diagnostics
    );
    let server = WebDevServer::bind_runtime(0, None, None, attachment.routes).unwrap();
    let response = send_raw_request(server.port, "GET /api HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    assert!(
        response.contains("Content-Type: application/json\r\n"),
        "{response}"
    );
    assert_eq!(response_body(&response), r#"{"ok":true}"#);
}

fn typed_record_json_route() -> RuntimeWebRoute {
    let source = r#"
type CreateUser = { name: String, age: Int }
fn create_user(input: CreateUser from json) -> String { input.name }
fn register() { perform Web.route("POST", "/users", create_user) }
"#;
    let (module, function_index) = compile_handler(source, "create_user");
    let ast = Parser::new(Lexer::new(source).lex().unwrap())
        .parse_module()
        .unwrap();
    let compilation = compile_module_contracts(&ast);
    assert!(
        compilation.diagnostics.is_empty(),
        "{:?}",
        compilation.diagnostics
    );
    let plan =
        compile_runtime_route_plan(&compilation.routes[0]).expect("compile JSON record binding");
    RuntimeWebRoute {
        route: WebRoute {
            method: HttpMethod::Post,
            path: "/users".to_string(),
            handler_module: module,
            handler_func_idx: function_index,
        },
        plan: Some(plan),
    }
}

#[test]
fn typed_json_record_is_materialized_for_real_vm_handler() {
    let server = WebDevServer::bind_runtime(0, None, None, vec![typed_record_json_route()])
        .expect("bind record route");
    let body = r#"{"name":"Ada","age":42}"#;
    let request = format!(
        "POST /users HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        body.len(), body
    );
    let response = send_raw_request(server.port, &request);
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    assert_eq!(response_body(&response), "Ada");
}

#[test]
fn typed_json_record_rejects_missing_wrong_typed_and_extra_fields() {
    let server = WebDevServer::bind_runtime(0, None, None, vec![typed_record_json_route()])
        .expect("bind record route");
    for body in [
        r#"{"name":"Ada"}"#,
        r#"{"name":123,"age":42}"#,
        r#"{"name":"Ada","age":"42"}"#,
        r#"{"name":"Ada","age":42,"is_admin":true}"#,
        r#"["Ada",42]"#,
        r#"{"name":"Ada","age":42,"age":30}"#,
    ] {
        let request = format!(
            "POST /users HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(), body
        );
        let response = send_raw_request(server.port, &request);
        assert!(
            response.starts_with("HTTP/1.1 400 Bad Request\r\n"),
            "body={body}, response={response}"
        );
        assert!(response.contains("application/problem+json"), "{response}");
    }
}

#[test]
fn typed_record_json_respects_non_declaration_field_ids() {
    let source = r#"
type CreateUser = { name: String, age: Int }
fn age_only(input: CreateUser from json) -> Int { input.age }
fn register() { perform Web.route("POST", "/age", age_only) }
"#;
    let (module, index) = compile_handler(source, "age_only");
    let ast = Parser::new(Lexer::new(source).lex().unwrap())
        .parse_module()
        .unwrap();
    let compilation = compile_module_contracts(&ast);
    let plan = compile_runtime_route_plan(&compilation.routes[0]).unwrap();
    let server = WebDevServer::bind_runtime(
        0,
        None,
        None,
        vec![RuntimeWebRoute {
            route: WebRoute {
                method: HttpMethod::Post,
                path: "/age".into(),
                handler_module: module,
                handler_func_idx: index,
            },
            plan: Some(plan),
        }],
    )
    .unwrap();
    let body = r#"{"name":"Ada","age":42}"#;
    let request = format!(
        "POST /age HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        body.len(), body
    );
    let response = send_raw_request(server.port, &request);
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    assert_eq!(response_body(&response), "42");
}

fn typed_json_response_route(source: &str, handler_name: &str, path: &str) -> RuntimeWebRoute {
    let (module, function_index) = compile_handler(source, handler_name);
    let ast = Parser::new(Lexer::new(source).lex().unwrap())
        .parse_module()
        .unwrap();
    let compiled = compile_module_contracts(&ast);
    assert!(
        compiled.diagnostics.is_empty(),
        "{:?}",
        compiled.diagnostics
    );
    assert_eq!(compiled.routes.len(), 1);
    let plan = compile_runtime_route_plan(&compiled.routes[0]).unwrap();
    RuntimeWebRoute {
        route: WebRoute {
            method: HttpMethod::Get,
            path: path.into(),
            handler_module: module,
            handler_func_idx: function_index,
        },
        plan: Some(plan),
    }
}

#[test]
fn typed_json_record_response_is_serialized_from_real_vm_value() {
    let source = r#"
type UserView = { name: String, age: Int }
fn show() -> UserView { { name: "Ada \"Lovelace\"", age: 42 } }
fn register() { perform Web.route_json_typed("GET", "/typed", show) }
"#;
    let route = typed_json_response_route(source, "show", "/typed");
    assert!(route
        .plan
        .as_ref()
        .unwrap()
        .response_type
        .as_deref()
        .is_some_and(|v| v.starts_with("Json[")));
    let server = WebDevServer::bind_runtime(0, None, None, vec![route]).unwrap();
    let response = send_raw_request(
        server.port,
        "GET /typed HTTP/1.1\r\nHost: localhost\r\n\r\n",
    );
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    assert!(
        response.contains("Content-Type: application/json\r\n"),
        "{response}"
    );
    let body: serde_json::Value = serde_json::from_str(response_body(&response)).unwrap();
    assert_eq!(
        body,
        serde_json::json!({"name":"Ada \"Lovelace\"","age":42})
    );
}

#[test]
fn typed_json_scalar_response_is_serialized_without_string_coercion() {
    let source = r#"
fn show() -> String { "hello \"world\"" }
fn register() { perform Web.route_json_typed("GET", "/typed-string", show) }
"#;
    let route = typed_json_response_route(source, "show", "/typed-string");
    let server = WebDevServer::bind_runtime(0, None, None, vec![route]).unwrap();
    let response = send_raw_request(
        server.port,
        "GET /typed-string HTTP/1.1\r\nHost: localhost\r\n\r\n",
    );
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    assert!(
        response.contains("Content-Type: application/json\r\n"),
        "{response}"
    );
    let body: serde_json::Value = serde_json::from_str(response_body(&response)).unwrap();
    assert_eq!(body, serde_json::Value::String("hello \"world\"".into()));
}

#[test]
fn typed_json_response_rejects_runtime_representation_mismatch() {
    let source = r#"
fn show() -> String { "not an object" }
fn register() { perform Web.route_json_typed("GET", "/mismatch", show) }
"#;
    let mut route = typed_json_response_route(source, "show", "/mismatch");
    route.plan.as_mut().unwrap().response_type = Some("Json[{ name: String, age: Int }]".into());
    let server = WebDevServer::bind_runtime(0, None, None, vec![route]).unwrap();
    let response = send_raw_request(
        server.port,
        "GET /mismatch HTTP/1.1\r\nHost: localhost\r\n\r\n",
    );
    assert!(
        response.starts_with("HTTP/1.1 500 Internal Server Error\r\n"),
        "{response}"
    );
    assert!(!response.contains("not an object"), "{response}");
}

#[test]
fn typed_json_output_serializes_real_boolean_and_integer() {
    for (path, source, expected) in [
        (
            "/bool",
            r#"fn show() -> Bool { true }
fn register() { perform Web.route_json_typed("GET", "/bool", show) }"#,
            "true",
        ),
        (
            "/int",
            r#"fn show() -> Int { 42 }
fn register() { perform Web.route_json_typed("GET", "/int", show) }"#,
            "42",
        ),
    ] {
        let route = typed_json_response_route(source, "show", path);
        let server = WebDevServer::bind_runtime(0, None, None, vec![route]).unwrap();
        let response = send_raw_request(
            server.port,
            &format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n"),
        );
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
        assert_eq!(response_body(&response), expected);
    }
}

#[test]
fn nested_json_response_serializes_embedded_vm_record() {
    let source = r#"
type User = { name: String, address: { city: String, zip: Int } }
fn show() -> User {
    { name: "Ada", address: { city: "London", zip: 12345 } }
}
fn register() { perform Web.route_json_typed("GET", "/nested", show) }
"#;
    let route = typed_json_response_route(source, "show", "/nested");
    let server = WebDevServer::bind_runtime(0, None, None, vec![route]).unwrap();
    let response = send_raw_request(
        server.port,
        "GET /nested HTTP/1.1\r\nHost: localhost\r\n\r\n",
    );
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    let body: serde_json::Value = serde_json::from_str(response_body(&response)).unwrap();
    assert_eq!(
        body,
        serde_json::json!({
            "name": "Ada", "address": { "city": "London", "zip": 12345 }
        })
    );
}

#[test]
fn nested_json_request_materializes_embedded_vm_record() {
    let source = r#"
type CreateUser = { name: String, address: { city: String, zip: Int } }
fn create_user(input: CreateUser from json) -> String { input.address.city }
fn register() { perform Web.route("POST", "/nested", create_user) }
"#;
    let (module, function_index) = compile_handler(source, "create_user");
    let ast = Parser::new(Lexer::new(source).lex().unwrap())
        .parse_module()
        .unwrap();
    let compilation = compile_module_contracts(&ast);
    assert!(
        compilation.diagnostics.is_empty(),
        "{:?}",
        compilation.diagnostics
    );
    let plan = compile_runtime_route_plan(&compilation.routes[0]).unwrap();
    let server = WebDevServer::bind_runtime(
        0,
        None,
        None,
        vec![RuntimeWebRoute {
            route: WebRoute {
                method: HttpMethod::Post,
                path: "/nested".into(),
                handler_module: module,
                handler_func_idx: function_index,
            },
            plan: Some(plan),
        }],
    )
    .unwrap();
    for (body, expected) in [
        (
            r#"{"name":"Ada","address":{"city":"London","zip":12345}}"#,
            200,
        ),
        (r#"{"name":"Ada","address":{"city":"London"}}"#, 400),
        (
            r#"{"name":"Ada","address":{"city":"London","zip":12345,"admin":true}}"#,
            400,
        ),
        (
            r#"{"name":"Ada","address":{"city":"London","zip":"12345"}}"#,
            400,
        ),
        (
            r#"{"name":"Ada","address":{"city":"London","zip":12345,"zip":42}}"#,
            400,
        ),
    ] {
        let request = format!(
            "POST /nested HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(), body);
        let response = send_raw_request(server.port, &request);
        let status = if response.starts_with("HTTP/1.1 200 OK") {
            200
        } else if response.starts_with("HTTP/1.1 400") {
            400
        } else {
            0
        };
        assert_eq!(status, expected, "body={body}; response={response}");
        if expected == 200 {
            assert_eq!(response_body(&response), "London");
        }
    }
}
