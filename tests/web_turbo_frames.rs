use nulang::ast::Decl;
use nulang::lexer::Lexer;
use nulang::parser::Parser;
use nulang::runtime::{HttpMethod, WebDevServer, WebRoute};
use nulang::typechecker::TypeChecker;
use std::collections::HashSet;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream};
use std::path::Path;
use std::time::Duration;

#[test]
fn turbo_frames_helpers_import_and_typecheck() {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("conformance/behavior/web_20_turbo_frames.nula");
    let source = std::fs::read_to_string(&path).expect("read Turbo Frame fixture");
    let tokens = Lexer::new(&source).lex().expect("lex");
    let mut ast = Parser::new(tokens).parse_module().expect("parse");
    let mut visited = HashSet::new();
    nulang::resolver::resolve_imports(&mut ast, &path, &mut visited)
        .expect("resolve Turbo Frame helpers without name collisions");
    let mut checker = TypeChecker::new();
    checker
        .check_module(&ast)
        .expect("typecheck Turbo Frame helpers");
    assert!(ast
        .decls
        .iter()
        .any(|decl| matches!(decl, Decl::Function { name, .. } if name == "frame")));
    assert!(ast
        .decls
        .iter()
        .any(|decl| matches!(decl, Decl::Function { name, .. } if name == "lazy_frame")));
}

fn compile_route(handler_source: &str, name: &str, route_path: &str) -> WebRoute {
    let tokens = Lexer::new(handler_source).lex().expect("lex handler");
    let mut ast = Parser::new(tokens).parse_module().expect("parse handler");
    let root_path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("conformance/behavior/web_20_turbo_frames.nula");
    let mut visited = HashSet::new();
    nulang::resolver::resolve_imports(&mut ast, &root_path, &mut visited)
        .expect("resolve handler imports");
    let mut checker = TypeChecker::new();
    checker.check_module(&ast).expect("typecheck handler");
    let hir = nulang::hir_lower::lower_module(&ast, &checker.inferred_decl_types);
    let mut mir = nulang::mir_lower::lower_module(&hir).expect("lower MIR");
    let module =
        nulang::mir_codegen::compile_mir(&mut mir, "frame-test").expect("compile bytecode");
    let offset = module
        .debug_functions
        .iter()
        .find(|function| function.name == name)
        .expect("handler exists")
        .code_offset;
    let index = module
        .function_table
        .iter()
        .position(|item| *item == offset)
        .expect("handler in function table");
    WebRoute {
        method: HttpMethod::Get,
        path: route_path.to_string(),
        handler_module: module,
        handler_func_idx: index,
    }
}

fn request(port: u16, path: &str) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream.write_all(format!(
        "GET {path} HTTP/1.1\r\nHost: localhost\r\nTurbo-Frame: activities\r\nConnection: close\r\n\r\n"
    ).as_bytes()).unwrap();
    stream.shutdown(Shutdown::Write).unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).expect("read response");
    String::from_utf8(response).unwrap()
}

#[test]
fn rendered_turbo_frame_preserves_attributes_and_escapes_untrusted_html() {
    let server = WebDevServer::bind(
        0,
        None,
        None,
        vec![compile_route(
            r#"fn show() {
    perform Web.html("turbo-frame", [
        ("id", perform Web.text("activities")),
        ("src", perform Web.text("/activities?page=2")),
        ("loading", perform Web.text("lazy"))
    ], [perform Web.text("Tea </turbo-frame><script>alert(1)</script>")])
}"#,
            "show",
            "/activities",
        )],
    )
    .expect("bind");
    let response = request(server.port, "/activities");
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    assert!(
        response.contains("Content-Type: text/html; charset=utf-8"),
        "{response}"
    );
    let html = response.split_once("\r\n\r\n").unwrap().1;
    assert!(html.starts_with("<turbo-frame id=\"activities\" src=\"/activities?page=2\" loading=\"lazy\">Tea &lt;/turbo-frame&gt;&lt;script&gt;alert(1)&lt;/script&gt;</turbo-frame>"), "{html}");
}

#[test]
fn frame_url_rejects_script_protocol() {
    let server = WebDevServer::bind(
        0,
        None,
        None,
        vec![compile_route(
            r#"fn show() {
    perform Web.html("turbo-frame", [
        ("id", perform Web.text("activities")),
        ("src", perform Web.text("javascript:alert(1)"))
    ], [])
}"#,
            "show",
            "/unsafe",
        )],
    )
    .expect("bind");
    let response = request(server.port, "/unsafe");
    // Keep this HTML-escaping test compatible with both baseline main and
    // the fail-closed handler-status fix proposed in #2583. Once #2583 is
    // merged, tighten this test to 500-only and remove the legacy case.
    let (head, body) = response.split_once("\r\n\r\n").unwrap();
    let legacy_result = head.starts_with("HTTP/1.1 200 OK") && body == "nil";
    let fixed_result =
        head.starts_with("HTTP/1.1 500 Internal Server Error") && body == "Internal server error";
    assert!(legacy_result || fixed_result, "{response}");
    assert!(!response.contains("javascript:alert"), "{response}");
    assert!(!response.contains("<turbo-frame"), "{response}");
}

#[test]
fn imported_frame_helper_renders_escaped_content_over_real_http() {
    let server = WebDevServer::bind(
        0,
        None,
        None,
        vec![compile_route(
            r#"import stdlib::web::frames
fn show() {
    frame("activities", [perform Web.text("A <script>alert(1)</script>")])
}"#,
            "show",
            "/imported",
        )],
    )
    .expect("bind");
    let response = request(server.port, "/imported");
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    let html = response.split_once("\r\n\r\n").unwrap().1;
    assert!(
        html.starts_with(
            "<turbo-frame id=\"activities\">A &lt;script&gt;alert(1)&lt;/script&gt;</turbo-frame>"
        ),
        "{html}"
    );
}
