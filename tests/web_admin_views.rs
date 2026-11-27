//! Read-only admin view helpers must compile and serve escaped HTML.
use nulang::lexer::Lexer;
use nulang::parser::Parser;
use nulang::runtime::{HttpMethod, WebDevServer, WebRoute};
use nulang::typechecker::TypeChecker;
use std::collections::HashSet;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream};
use std::path::Path;
use std::time::Duration;

fn compile_view(source: &str) -> WebRoute {
    let mut ast = Parser::new(Lexer::new(source).lex().expect("lex source"))
        .parse_module()
        .expect("parse view");
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("conformance/behavior/web_21_admin_views.nula");
    let mut visited = HashSet::new();
    nulang::resolver::resolve_imports(&mut ast, &path, &mut visited)
        .expect("resolve admin view helpers");
    let mut checker = TypeChecker::new();
    checker.check_module(&ast).expect("typecheck view");
    let hir = nulang::hir_lower::lower_module(&ast, &checker.inferred_decl_types);
    let mut mir = nulang::mir_lower::lower_module(&hir).expect("lower MIR");
    let module =
        nulang::mir_codegen::compile_mir(&mut mir, "admin-view-test").expect("compile bytecode");
    let offset = module
        .debug_functions
        .iter()
        .find(|f| f.name == "show")
        .expect("show handler exists")
        .code_offset;
    let index = module
        .function_table
        .iter()
        .position(|entry| *entry == offset)
        .expect("show in function table");
    WebRoute {
        method: HttpMethod::Get,
        path: "/admin-preview".to_string(),
        handler_module: module,
        handler_func_idx: index,
    }
}

fn get_response(source: &str) -> String {
    let server = WebDevServer::bind(0, None, None, vec![compile_view(source)])
        .expect("start real HTTP server");
    let mut stream = TcpStream::connect(("127.0.0.1", server.port)).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .write_all(b"GET /admin-preview HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .unwrap();
    stream.shutdown(Shutdown::Write).unwrap();
    let mut bytes = Vec::new();
    stream
        .read_to_end(&mut bytes)
        .expect("receive HTTP response");
    String::from_utf8(bytes).expect("UTF-8 response")
}

#[test]
fn readonly_table_renders_accessible_headers_and_escaped_cells() {
    let response = get_response(
        r#"
        import stdlib::web::admin
        fn show() { readonly_table(["Name", "Status"], [["Alice <script>alert(1)</script>", "Active"]]) }
    "#,
    );
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    assert!(response.contains("<table"), "{response}");
    assert!(
        response.contains("<th scope=\"col\">Name</th>"),
        "{response}"
    );
    assert!(
        response.contains("<td>Alice &lt;script&gt;alert(1)&lt;/script&gt;</td>"),
        "{response}"
    );
    assert!(!response.contains("<td>Alice <script>"), "{response}");
}

#[test]
fn readonly_table_rejects_ragged_rows_without_rendering_data() {
    let response = get_response(
        r#"
        import stdlib::web::admin
        fn show() { readonly_table(["Name"], [["Alice", "SECRET-EXTRA-CELL"]]) }
    "#,
    );
    assert!(response.contains("Invalid table data"), "{response}");
    assert!(!response.contains("SECRET-EXTRA-CELL"), "{response}");
}

#[test]
fn readonly_detail_view_escapes_field_values() {
    let response = get_response(
        r#"
        import stdlib::web::admin
        fn show() { readonly_detail(["Title", "Owner"], ["North <b>board</b>", "Alice"] ) }
    "#,
    );
    assert!(response.contains("<dl>"), "{response}");
    assert!(response.contains("<dt>Title</dt>"), "{response}");
    assert!(
        response.contains("<dd>North &lt;b&gt;board&lt;/b&gt;</dd>"),
        "{response}"
    );
}

#[test]
fn detail_view_rejects_mismatched_field_lengths() {
    let response = get_response(
        r#"
        import stdlib::web::admin
        fn show() { readonly_detail(["Name"], ["Alice", "SECRET-EXTRA-VALUE"]) }
    "#,
    );
    assert!(response.contains("Invalid detail data"), "{response}");
    assert!(!response.contains("SECRET-EXTRA-VALUE"), "{response}");
}
