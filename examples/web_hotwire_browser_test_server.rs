//! Test-only actual WebDevServer fixture for official Hotwire browser E2E.
//! Run with a temporary static root containing index.html and turbo.js.
//! Do not use this executable for production service or user data.

use nulang::lexer::Lexer;
use nulang::parser::Parser;
use nulang::runtime::{HttpMethod, WebDevServer, WebRoute};
use nulang::typechecker::TypeChecker;
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

fn route(method: HttpMethod, path: &str, name: &str, source: &str) -> WebRoute {
    let tokens = Lexer::new(source).lex().expect("lex handler");
    let ast = Parser::new(tokens).parse_module().expect("parse handler");
    let mut types = TypeChecker::new();
    types.check_module(&ast).expect("typecheck handler");
    let hir = nulang::hir_lower::lower_module(&ast, &types.inferred_decl_types);
    let mut mir = nulang::mir_lower::lower_module(&hir).expect("lower MIR");
    let module =
        nulang::mir_codegen::compile_mir(&mut mir, "hotwire-e2e").expect("compile bytecode");
    let offset = module
        .debug_functions
        .iter()
        .find(|func| func.name == name)
        .expect("handler exists")
        .code_offset;
    let index = module
        .function_table
        .iter()
        .position(|offset2| *offset2 == offset)
        .expect("handler index");
    WebRoute {
        method,
        path: path.into(),
        handler_module: module,
        handler_func_idx: index,
    }
}

fn main() {
    let static_dir = PathBuf::from(std::env::args().nth(1).expect("static root required"));
    let routes = vec![
        route(
            HttpMethod::Get,
            "/recent",
            "recent",
            r#"fn recent() {
            perform Web.html("turbo-frame", [("id", perform Web.text("recent"))], [
                perform Web.text("Tea & toast")
            ])
        }"#,
        ),
        route(
            HttpMethod::Post,
            "/todos",
            "create",
            r#"fn create() {
            let fragment = perform Web.html("li", [("id", perform Web.text("todo-2"))], [
                perform Web.text("New <script>window.__pwned=true</script>")
            ])
            perform Web.turbo_stream("append", "todos", fragment)
        }"#,
        ),
        route(
            HttpMethod::Post,
            "/todos/2/edit",
            "update",
            r#"fn update() {
            let fragment = perform Web.html("li", [("id", perform Web.text("todo-2"))], [
                perform Web.text("Updated")
            ])
            perform Web.turbo_stream("replace", "todo-2", fragment)
        }"#,
        ),
        route(
            HttpMethod::Post,
            "/todos/2/delete",
            "delete",
            r#"fn delete() {
            perform Web.turbo_stream("remove", "todo-2", perform Web.text(""))
        }"#,
        ),
    ];
    let server = WebDevServer::bind(0, Some(static_dir), None, routes).expect("WebDevServer");
    println!("PORT={}", server.port);
    std::io::stdout().flush().unwrap();
    loop {
        std::thread::sleep(Duration::from_secs(30));
    }
}
