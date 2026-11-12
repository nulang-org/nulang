//! Real TCP proofs for opt-in server-verified tenant admission.
use nulang::lexer::Lexer;
use nulang::parser::Parser;
use nulang::runtime::{HttpMethod, WebDevServer, WebRoute};
use nulang::typechecker::TypeChecker;
use nulang::web::tenant_auth::{TenantAuthGuard, VerifiedTenant};
use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::Arc;
use std::time::Duration;

const SESSION_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const SESSION_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn route(path: &str) -> WebRoute {
    let source = r#"
        import stdlib::web::host
        fn secure() { perform Web.text(perform Web.verified_tenant_id()) }
    "#;
    let mut module = Parser::new(Lexer::new(source).lex().unwrap())
        .parse_module()
        .unwrap();
    let mut visited = std::collections::HashSet::new();
    nulang::resolver::resolve_imports(
        &mut module,
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("conformance/behavior/web_21_admin_views.nula"),
        &mut visited,
    )
    .unwrap();
    let mut checker = TypeChecker::new();
    checker.check_module(&module).unwrap();
    let hir = nulang::hir_lower::lower_module(&module, &checker.inferred_decl_types);
    let mut mir = nulang::mir_lower::lower_module(&hir).unwrap();
    let compiled = nulang::mir_codegen::compile_mir(&mut mir, "authenticated-http").unwrap();
    let offset = compiled
        .debug_functions
        .iter()
        .find(|f| f.name == "secure")
        .unwrap()
        .code_offset;
    let idx = compiled
        .function_table
        .iter()
        .position(|value| *value == offset)
        .unwrap();
    WebRoute {
        method: HttpMethod::Get,
        path: path.into(),
        handler_module: compiled,
        handler_func_idx: idx,
    }
}

fn guard() -> TenantAuthGuard {
    let resolver = Arc::new(|session: &str| match session {
        SESSION_A => VerifiedTenant::new("user-a", "tenant-a"),
        SESSION_B => VerifiedTenant::new("user-b", "tenant-b"),
        _ => None,
    });
    TenantAuthGuard::new(
        "__Host-nulang_tenant",
        vec![(HttpMethod::Get, "/secure".into())],
        resolver,
    )
    .unwrap()
}

fn get(port: u16, path: &str, headers: &[(&str, String)]) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut request = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n");
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("\r\n");
    stream.write_all(request.as_bytes()).unwrap();
    stream.shutdown(Shutdown::Write).unwrap();
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes).unwrap();
    String::from_utf8(bytes).unwrap()
}

#[test]
fn valid_opaque_session_exposes_only_server_resolved_tenant_id() {
    let server = WebDevServer::bind_with_tenant_guard(
        0,
        None,
        None,
        vec![route("/secure"), route("/public")],
        guard(),
    )
    .unwrap();
    let a = get(
        server.port,
        "/secure",
        &[
            ("Cookie", format!("__Host-nulang_tenant={SESSION_A}")),
            ("X-Tenant-Id", "attacker".into()),
        ],
    );
    assert!(a.starts_with("HTTP/1.1 200 OK"), "{a}");
    assert!(a.ends_with("tenant-a"), "{a}");
    assert!(!a.contains("attacker"), "{a}");
    let b = get(
        server.port,
        "/secure",
        &[("Cookie", format!("__Host-nulang_tenant={SESSION_B}"))],
    );
    assert!(b.ends_with("tenant-b"), "{b}");
    let public = get(
        server.port,
        "/public",
        &[("Cookie", format!("__Host-nulang_tenant={SESSION_A}"))],
    );
    assert!(public.starts_with("HTTP/1.1 200 OK"), "{public}");
    assert!(!public.contains("tenant-a"), "{public}");
}

#[test]
fn protected_route_rejects_missing_forged_duplicate_and_malformed_sessions_before_dispatch() {
    let server =
        WebDevServer::bind_with_tenant_guard(0, None, None, vec![route("/secure")], guard())
            .unwrap();
    let attempts = [
        vec![],
        vec![("Cookie", "__Host-nulang_tenant=forged".to_string())],
        vec![(
            "Cookie",
            "__Host-nulang_tenant=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
                .to_string(),
        )],
        vec![(
            "Cookie",
            format!("__Host-nulang_tenant={SESSION_A}; __Host-nulang_tenant={SESSION_B}"),
        )],
        vec![
            ("Cookie", format!("__Host-nulang_tenant={SESSION_A}")),
            ("Cookie", format!("__Host-nulang_tenant={SESSION_B}")),
        ],
    ];
    for headers in attempts {
        let response = get(server.port, "/secure", &headers);
        assert!(
            response.starts_with("HTTP/1.1 401 Unauthorized"),
            "{response}"
        );
        assert!(
            !response.contains("tenant-a") && !response.contains("tenant-b"),
            "{response}"
        );
        assert!(response.contains("Cache-Control: no-store"), "{response}");
    }
}

#[test]
fn unknown_or_duplicate_guarded_route_configuration_is_rejected_at_startup() {
    assert!(
        WebDevServer::bind_with_tenant_guard(0, None, None, vec![route("/public")], guard())
            .is_err()
    );
    assert!(WebDevServer::bind_with_tenant_guard(
        0,
        None,
        None,
        vec![route("/secure"), route("/secure")],
        guard()
    )
    .is_err());
}

#[test]
fn protected_route_cannot_be_shadowed_by_an_earlier_unguarded_parameter_route() {
    // /:slug would intercept GET /secure before the guarded literal route.
    // Startup must reject overlapping route registrations rather than permit
    // an attacker to bypass authentication by relying on router order.
    let routes = vec![route("/:slug"), route("/secure")];
    assert!(WebDevServer::bind_with_tenant_guard(0, None, None, routes, guard()).is_err());
}

#[test]
fn unsafe_protected_route_cannot_start_without_registered_csrf_form_guard() {
    let mut mutation = route("/mutate");
    mutation.method = HttpMethod::Post;
    let resolver = Arc::new(|token: &str| {
        if token == SESSION_A {
            VerifiedTenant::new("user-a", "tenant-a")
        } else {
            None
        }
    });
    let tenant_guard = TenantAuthGuard::new(
        "__Host-nulang_tenant",
        vec![(HttpMethod::Post, "/mutate".into())],
        resolver,
    )
    .unwrap();
    assert!(
        WebDevServer::bind_with_tenant_guard(0, None, None, vec![mutation], tenant_guard).is_err(),
        "a server-owned tenant session is not a replacement for mutation CSRF admission"
    );
}
