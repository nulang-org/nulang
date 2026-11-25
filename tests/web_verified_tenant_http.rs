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

#[test]
fn policy_admission_denies_authenticated_but_unauthorized_requests_before_handler() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    let enabled = Arc::new(AtomicBool::new(true));
    let calls = Arc::new(AtomicUsize::new(0));
    let live = Arc::clone(&enabled);
    let count = Arc::clone(&calls);
    let guard = guard().with_authorizer(Arc::new(move |principal, method, route, params| {
        count.fetch_add(1, Ordering::SeqCst);
        live.load(Ordering::SeqCst)
            && principal.subject_id() == "user-a"
            && principal.tenant_id() == "tenant-a"
            && method == HttpMethod::Get
            && route == "/secure"
            && params.is_empty()
    }));
    let server = WebDevServer::bind_with_tenant_guard(
        0,
        None,
        None,
        vec![route("/secure"), route("/public")],
        guard,
    )
    .unwrap();
    let a = [("Cookie", format!("__Host-nulang_tenant={SESSION_A}"))];
    let b = [
        ("Cookie", format!("__Host-nulang_tenant={SESSION_B}")),
        ("X-Tenant-Id", "tenant-a".into()),
        ("X-Role", "owner".into()),
    ];
    let anonymous = get(server.port, "/secure", &[]);
    assert!(
        anonymous.starts_with("HTTP/1.1 401 Unauthorized"),
        "{anonymous}"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let allowed = get(server.port, "/secure", &a);
    assert!(
        allowed.starts_with("HTTP/1.1 200 OK") && allowed.ends_with("tenant-a"),
        "{allowed}"
    );
    assert!(allowed.contains("Cache-Control: no-store"), "{allowed}");
    let denied = get(server.port, "/secure", &b);
    assert!(denied.starts_with("HTTP/1.1 403 Forbidden"), "{denied}");
    assert!(
        denied.contains("Cache-Control: no-store") && !denied.contains("tenant-b"),
        "{denied}"
    );
    enabled.store(false, Ordering::SeqCst);
    let revoked = get(server.port, "/secure", &a);
    assert!(revoked.starts_with("HTTP/1.1 403 Forbidden"), "{revoked}");
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    let public = get(server.port, "/public", &a);
    assert!(public.starts_with("HTTP/1.1 200 OK"), "{public}");
    assert!(!public.contains("tenant-a"), "{public}");
    assert_eq!(calls.load(Ordering::SeqCst), 3);
}

#[test]
fn tenant_admission_and_custom_host_options_share_one_startup_gate() {
    use nulang::runtime::WebServerOptions;
    use nulang::web::runtime_bindings::RuntimeWebRoute;
    let routes = || {
        vec![RuntimeWebRoute {
            route: route("/secure"),
            plan: None,
        }]
    };
    let policy_guard = || {
        guard().with_authorizer(Arc::new(|principal, _, _, _| {
            principal.subject_id() == "user-a"
        }))
    };
    let invalid = WebServerOptions {
        max_concurrent_connections: 0,
        ..WebServerOptions::default()
    };
    assert!(WebDevServer::bind_runtime_with_tenant_guard_and_options(
        0,
        None,
        None,
        routes(),
        vec![],
        policy_guard(),
        invalid
    )
    .is_err());
    let options = WebServerOptions {
        max_concurrent_connections: 2,
        ..WebServerOptions::default()
    };
    let server = WebDevServer::bind_runtime_with_tenant_guard_and_options(
        0,
        None,
        None,
        routes(),
        vec![],
        policy_guard(),
        options,
    )
    .unwrap();
    let permitted = get(
        server.port,
        "/secure",
        &[("Cookie", format!("__Host-nulang_tenant={SESSION_A}"))],
    );
    assert!(permitted.starts_with("HTTP/1.1 200 OK"), "{permitted}");
    let denied = get(
        server.port,
        "/secure",
        &[("Cookie", format!("__Host-nulang_tenant={SESSION_B}"))],
    );
    assert!(denied.starts_with("HTTP/1.1 403 Forbidden"), "{denied}");
    let anonymous = get(server.port, "/secure", &[]);
    assert!(
        anonymous.starts_with("HTTP/1.1 401 Unauthorized"),
        "{anonymous}"
    );
}

#[test]
fn native_permissions_authorizer_enforces_current_trusted_resource_policy() {
    use nulang::web::native_permissions::{
        NativePermissions, PermissionRule, PolicyInput, RoleAssignment,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    let mode = Arc::new(AtomicUsize::new(0));
    let policy_mode = mode.clone();
    let native = NativePermissions::compile().expect("compile bundled policy evaluator");
    let authorizer = native.authorizer(Arc::new(move |_, method, template, params| {
        if method != HttpMethod::Get || template != "/secure/:id" {
            return None;
        }
        let resource_tenant_id = match params.get("id").map(String::as_str) {
            Some("a") => "tenant-a",
            Some("b") => "tenant-b",
            _ => return None,
        };
        let mut input = PolicyInput {
            resource_tenant_id: resource_tenant_id.into(),
            resource: "document".into(),
            action: "read".into(),
            assignments: vec![RoleAssignment {
                tenant_id: "tenant-a".into(),
                subject_id: "user-a".into(),
                role: "reader".into(),
            }],
            rules: vec![PermissionRule {
                tenant_id: "tenant-a".into(),
                role: "reader".into(),
                resource: "document".into(),
                action: "read".into(),
                outcome: "allow".into(),
            }],
        };
        match policy_mode.load(Ordering::SeqCst) {
            1 => input.assignments.clear(),
            2 => input.rules.push(PermissionRule {
                outcome: "deny".into(),
                ..input.rules[0].clone()
            }),
            3 => input.rules.push(PermissionRule {
                outcome: "malformed".into(),
                ..input.rules[0].clone()
            }),
            4 => input.rules = vec![input.rules[0].clone(); 129],
            5 => return None,
            _ => {}
        }
        Some(input)
    }));
    let resolver = Arc::new(|session: &str| match session {
        SESSION_A => VerifiedTenant::new("user-a", "tenant-a"),
        SESSION_B => VerifiedTenant::new("user-b", "tenant-b"),
        _ => None,
    });
    let guard = TenantAuthGuard::new(
        "__Host-nulang_tenant",
        vec![(HttpMethod::Get, "/secure/:id".into())],
        resolver,
    )
    .unwrap()
    .with_authorizer(authorizer);
    let server =
        WebDevServer::bind_with_tenant_guard(0, None, None, vec![route("/secure/:id")], guard)
            .unwrap();
    let a = [("Cookie", format!("__Host-nulang_tenant={SESSION_A}"))];
    let b = [
        ("Cookie", format!("__Host-nulang_tenant={SESSION_B}")),
        ("X-Role", "reader".into()),
        ("X-Tenant-Id", "tenant-a".into()),
    ];
    let allowed = get(server.port, "/secure/a", &a);
    assert!(allowed.starts_with("HTTP/1.1 200 OK"), "{allowed}");
    for (path, headers) in [
        ("/secure/b", &a[..]),
        ("/secure/a", &b[..]),
        ("/secure/missing", &a[..]),
    ] {
        let denied = get(server.port, path, headers);
        assert!(denied.starts_with("HTTP/1.1 403 Forbidden"), "{denied}");
        assert!(
            !denied.contains("tenant_mismatch") && !denied.contains("no_membership"),
            "{denied}"
        );
    }
    for value in 1..=5 {
        mode.store(value, Ordering::SeqCst);
        let denied = get(server.port, "/secure/a", &a);
        assert!(
            denied.starts_with("HTTP/1.1 403 Forbidden"),
            "mode {value}: {denied}"
        );
    }
}

#[test]
fn native_policy_snapshots_support_large_policies_and_preserve_late_denies() {
    use nulang::web::native_permissions::{
        NativePermissions, PermissionRule, PolicySnapshot, RoleAssignment, SnapshotPolicyInput,
    };
    use std::sync::RwLock;
    let membership = RoleAssignment {
        tenant_id: "tenant-a".into(),
        subject_id: "user-a".into(),
        role: "reader".into(),
    };
    let rule = PermissionRule {
        tenant_id: "tenant-a".into(),
        role: "reader".into(),
        resource: "document".into(),
        action: "read".into(),
        outcome: "allow".into(),
    };
    let mut assignments = vec![
        RoleAssignment {
            subject_id: "unrelated".into(),
            ..membership.clone()
        };
        127
    ];
    assignments.push(membership.clone());
    let mut rules = vec![
        PermissionRule {
            resource: "unrelated".into(),
            ..rule.clone()
        };
        127
    ];
    rules.push(rule.clone());
    let first =
        Arc::new(PolicySnapshot::new("tenant-a", 1, assignments.clone(), rules.clone()).unwrap());
    assert_eq!(first.revision(), 1);
    assert!(PolicySnapshot::new("tenant-a", 0, assignments.clone(), rules.clone()).is_err());
    assert!(PolicySnapshot::new("tenant-b", 1, assignments.clone(), rules.clone()).is_err());
    assert!(
        PolicySnapshot::new("tenant-a", 2, vec![membership.clone(); 129], rules.clone()).is_err()
    );
    assert!(
        PolicySnapshot::new("tenant-a", 2, assignments.clone(), vec![rule.clone(); 129]).is_err()
    );
    let current = Arc::new(RwLock::new(first.clone()));
    let store = current.clone();
    let native = NativePermissions::compile().unwrap();
    let authorizer = native.snapshot_authorizer(Arc::new(move |_, _, _, _| {
        Some(SnapshotPolicyInput {
            resource_tenant_id: "tenant-a".into(),
            resource: "document".into(),
            action: "read".into(),
            snapshot: store.read().ok()?.clone(),
        })
    }));
    let server = WebDevServer::bind_with_tenant_guard(
        0,
        None,
        None,
        vec![route("/secure")],
        guard().with_authorizer(authorizer),
    )
    .unwrap();
    let a = [("Cookie", format!("__Host-nulang_tenant={SESSION_A}"))];
    let b = [("Cookie", format!("__Host-nulang_tenant={SESSION_B}"))];
    let allowed = get(server.port, "/secure", &a);
    assert!(allowed.starts_with("HTTP/1.1 200 OK"), "{allowed}");
    let foreign = get(server.port, "/secure", &b);
    assert!(foreign.starts_with("HTTP/1.1 403 Forbidden"), "{foreign}");
    rules[126] = rule.clone();
    rules[127] = PermissionRule {
        outcome: "deny".into(),
        ..rule.clone()
    };
    let second = Arc::new(PolicySnapshot::new("tenant-a", 2, assignments, rules).unwrap());
    *current.write().unwrap() = second;
    let denied = get(server.port, "/secure", &a);
    assert!(denied.starts_with("HTTP/1.1 403 Forbidden"), "{denied}");
    assert_eq!(first.revision(), 1);
}
