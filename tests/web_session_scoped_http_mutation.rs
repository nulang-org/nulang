//! A real TCP dispatch regression for host-verified, tenant-authorized scopes.
//! No credential or tenant identity may originate in HTTP request fields.
use nulang::bytecode::CodeModule;
use nulang::lexer::Lexer;
use nulang::parser::Parser;
use nulang::runtime::{HttpMethod, WebDevServer, WebRoute};
use nulang::typechecker::TypeChecker;
use nulang::web::contracts::compile_module_contracts;
use nulang::web::csrf::SessionCsrfToken;
use nulang::web::runtime_bindings::{compile_runtime_route_plan, RuntimeWebRoute};
use nulang::web::session_preflight::{
    ResourceReadAuthorization, ResourceWriteAuthorization, SessionLookup, SessionMutationGateway,
    SessionReadGateway, TrustedSession, SESSION_COOKIE_NAME,
};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const HANDLE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn route(method: &str) -> RuntimeWebRoute {
    let body =
        "fn show(resource_id: String, principal: String, tenant: String) -> String { tenant }";
    let ast = Parser::new(Lexer::new(body).lex().unwrap())
        .parse_module()
        .unwrap();
    let mut types = TypeChecker::new();
    types.check_module(&ast).unwrap();
    let hir = nulang::hir_lower::lower_module(&ast, &types.inferred_decl_types);
    let mut mir = nulang::mir_lower::lower_module(&hir).unwrap();
    let module: CodeModule = nulang::mir_codegen::compile_mir(&mut mir, "scope-http").unwrap();
    let offset = module
        .debug_functions
        .iter()
        .find(|f| f.name == "show")
        .unwrap()
        .code_offset;
    let idx = module
        .function_table
        .iter()
        .position(|n| *n == offset)
        .unwrap();
    let source = format!(
        r#"
fn show(resource_id: String from path,
        principal: String from scope("principal_id"),
        tenant: String from scope("tenant_id")) -> String {{ tenant }}
fn web_main() {{ perform Web.route("{method}", "/accounts/{{resource_id}}", show) }}
"#
    );
    let ast = Parser::new(Lexer::new(&source).lex().unwrap())
        .parse_module()
        .unwrap();
    let contracts = compile_module_contracts(&ast);
    assert!(
        contracts.diagnostics.is_empty(),
        "{:?}",
        contracts.diagnostics
    );
    let plan = compile_runtime_route_plan(&contracts.routes[0]).unwrap();
    RuntimeWebRoute {
        route: WebRoute {
            method: HttpMethod::from_str(method).unwrap(),
            path: "/accounts/{resource_id}".into(),
            handler_module: module,
            handler_func_idx: idx,
        },
        plan: Some(plan),
    }
}

struct Store {
    session: Option<TrustedSession>,
    fail: bool,
    calls: Arc<AtomicUsize>,
}
impl SessionLookup for Store {
    fn lookup(&self, handle: &str) -> Result<Option<TrustedSession>, ()> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.fail {
            Err(())
        } else if handle == HANDLE {
            Ok(self.session.clone())
        } else {
            Ok(None)
        }
    }
}
struct Policy {
    allowed: bool,
    calls: Arc<AtomicUsize>,
}
impl ResourceReadAuthorization for Policy {
    fn can_read(&self, principal: &str, tenant: &str, resource: &str) -> Result<bool, ()> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        assert_eq!(principal, "principal-a");
        assert_eq!(tenant, "tenant-a");
        Ok(self.allowed && resource == "account-7")
    }
}
fn gateway(
    expired: bool,
    fail: bool,
    allowed: bool,
) -> (Arc<SessionReadGateway>, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let session = TrustedSession::new(
        "principal-a",
        "tenant-a",
        if expired { now - 1 } else { now + 120 },
        SessionCsrfToken::generate().unwrap(),
    );
    let store_calls = Arc::new(AtomicUsize::new(0));
    let policy_calls = Arc::new(AtomicUsize::new(0));
    let store = Store {
        session: Some(session),
        fail,
        calls: Arc::clone(&store_calls),
    };
    let policy = Policy {
        allowed,
        calls: Arc::clone(&policy_calls),
    };
    let gateway = SessionReadGateway::new(
        Arc::new(store),
        Arc::new(policy),
        Arc::new(|_, captures: &HashMap<String, String>| {
            captures
                .get("resource_id")
                .map(|id| ("tenant-a".to_string(), id.clone()))
        }),
    );
    (Arc::new(gateway), store_calls, policy_calls)
}
fn request(port: u16, raw: &str) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream.write_all(raw.as_bytes()).unwrap();
    stream.shutdown(Shutdown::Write).unwrap();
    let mut result = Vec::new();
    stream.read_to_end(&mut result).unwrap();
    String::from_utf8(result).unwrap()
}
struct WritePolicy {
    allowed: bool,
    fail: bool,
    calls: Arc<AtomicUsize>,
}
impl ResourceWriteAuthorization for WritePolicy {
    fn can_mutate(
        &self,
        principal: &str,
        tenant: &str,
        resource: &str,
        method: &str,
        route_pattern: &str,
    ) -> Result<bool, ()> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        assert_eq!(principal, "principal-a");
        assert_eq!(tenant, "tenant-a");
        assert_eq!(resource, "account-7");
        assert_eq!(method, "POST");
        assert_eq!(route_pattern, "/accounts/{resource_id}");
        if self.fail {
            Err(())
        } else {
            Ok(self.allowed)
        }
    }
}
fn mutation_gate(
    expired: bool,
    fail_store: bool,
    allowed: bool,
    fail_policy: bool,
    other_tenant: bool,
) -> (
    Arc<SessionReadGateway>,
    Arc<SessionMutationGateway>,
    String,
    Arc<AtomicUsize>,
    Arc<AtomicUsize>,
) {
    let (reads, _, _) = gateway(false, false, true);
    let csrf = SessionCsrfToken::generate().unwrap();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let session = TrustedSession::new(
        "principal-a",
        if other_tenant { "tenant-b" } else { "tenant-a" },
        if expired { now - 1 } else { now + 120 },
        csrf.clone(),
    );
    let lookups = Arc::new(AtomicUsize::new(0));
    let writes = Arc::new(AtomicUsize::new(0));
    let store = Store {
        session: Some(session),
        fail: fail_store,
        calls: lookups.clone(),
    };
    let policy = WritePolicy {
        allowed,
        fail: fail_policy,
        calls: writes.clone(),
    };
    let write_gateway = SessionMutationGateway::new(
        Arc::new(store),
        Arc::new(policy),
        Arc::new(|_, params: &HashMap<String, String>| {
            params
                .get("resource_id")
                .map(|id| ("tenant-a".into(), id.clone()))
        }),
    );
    (
        reads,
        Arc::new(write_gateway),
        csrf.encode(),
        lookups,
        writes,
    )
}
fn start(
    expired: bool,
    fail_store: bool,
    allowed: bool,
    fail_policy: bool,
    other_tenant: bool,
) -> (WebDevServer, String, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let (reads, writes, csrf, lookups, checks) =
        mutation_gate(expired, fail_store, allowed, fail_policy, other_tenant);
    let server = WebDevServer::bind_runtime_with_session_mutations(
        0,
        None,
        None,
        vec![route("POST")],
        "http://localhost",
        reads,
        writes,
    )
    .unwrap();
    (server, csrf, lookups, checks)
}
fn post(port: u16, method: &str, header_lines: &str, body: &str) -> String {
    request(port,&format!(
        "{method} /accounts/account-7?tenant_id=attacker HTTP/1.1\r\nHost: localhost\r\n{header_lines}Content-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()
    ))
}
fn headers(csrf: Option<&str>) -> String {
    let mut h=format!(
        "Origin: http://localhost\r\nSec-Fetch-Site: same-origin\r\nCookie: {SESSION_COOKIE_NAME}={HANDLE}; tenant_id=attacker\r\nX-Tenant-Id: attacker\r\n"
    );
    if let Some(csrf) = csrf {
        h.push_str(&format!("X-CSRF-Token: {csrf}\r\n"));
    }
    h
}
#[test]
fn server_owned_csrf_and_write_authority_allow_compiler_scoped_post() {
    let (server, csrf, lookups, writes) = start(false, false, true, false, false);
    let response = post(
        server.port,
        "POST",
        &headers(Some(&csrf)),
        "tenant_id=attacker",
    );
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    assert!(response.contains("\r\n\r\ntenant-a"), "{response}");
    assert_eq!(lookups.load(Ordering::SeqCst), 1);
    assert_eq!(writes.load(Ordering::SeqCst), 1);
}
#[test]
fn duplicate_invalid_and_missing_csrf_deny_before_write_authorizer() {
    for kind in 0..5 {
        let (server, csrf, _, writes) = start(false, false, true, false, false);
        let mut h = headers(Some(&csrf));
        match kind {
            0 => h = headers(None),
            1 => h.push_str(&format!("X-CSRF-Token: {csrf}\r\n")),
            2 => {
                h = h.replace(
                    &csrf,
                    "0000000000000000000000000000000000000000000000000000000000000000",
                )
            }
            3 => h = headers(None),
            _ => {}
        }
        let body = if kind == 3 {
            format!("__nulang_csrf={csrf}&__nulang_csrf={csrf}")
        } else if kind == 4 {
            format!("__nulang_csrf={csrf}")
        } else {
            "field=value".into()
        };
        let response = post(server.port, "POST", &h, &body);
        assert!(response.starts_with("HTTP/1.1 403 "), "{response}");
        assert_eq!(writes.load(Ordering::SeqCst), 0);
    }
}
#[test]
fn form_only_csrf_is_accepted() {
    let (server, csrf, lookups, writes) = start(false, false, true, false, false);
    let response = post(
        server.port,
        "POST",
        &headers(None),
        &format!("__nulang_csrf={csrf}&field=value"),
    );
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    assert_eq!(lookups.load(Ordering::SeqCst), 1);
    assert_eq!(writes.load(Ordering::SeqCst), 1);
}
#[test]
fn denied_expired_cross_tenant_and_unavailable_admission_never_runs_handler() {
    for (expired, store_fail, allowed, policy_fail, wrong_tenant, expected, expected_writes) in [
        (false, false, false, false, false, 403, 1),
        (false, false, true, true, false, 503, 1),
        (true, false, true, false, false, 401, 0),
        (false, true, true, false, false, 503, 0),
        (false, false, true, false, true, 403, 0),
    ] {
        let (server, csrf, _, writes) =
            start(expired, store_fail, allowed, policy_fail, wrong_tenant);
        let response = post(server.port, "POST", &headers(Some(&csrf)), "field=value");
        assert!(
            response.starts_with(&format!("HTTP/1.1 {expected} ")),
            "{response}"
        );
        assert_eq!(writes.load(Ordering::SeqCst), expected_writes);
        assert!(!response.contains("tenant-a"));
    }
}
#[test]
fn invalid_browser_provenance_denied_before_session_access() {
    for provenance in [
        "Origin: https://attacker.example\r\nSec-Fetch-Site: cross-site\r\n",
        "Sec-Fetch-Site: cross-site\r\n",
        "",
        "Origin: http://localhost\r\nOrigin: http://localhost\r\n",
        "Referer: https://evil.example/\r\n",
    ] {
        let (server, csrf, lookups, writes) = start(false, false, true, false, false);
        let h = format!(
            "Cookie: {SESSION_COOKIE_NAME}={HANDLE}\r\nX-CSRF-Token: {csrf}\r\n{provenance}"
        );
        let response = post(server.port, "POST", &h, "field=value");
        assert!(response.starts_with("HTTP/1.1 403 "), "{response}");
        assert_eq!(lookups.load(Ordering::SeqCst), 0);
        assert_eq!(writes.load(Ordering::SeqCst), 0);
    }
}
#[test]
fn read_only_server_blocks_post_even_when_cookie_and_csrf_valid() {
    let (reads, _, _) = gateway(false, false, true);
    let server = WebDevServer::bind_runtime_with_session_reads(
        0,
        None,
        None,
        vec![route("POST")],
        "localhost",
        reads,
    )
    .unwrap();
    let response = post(server.port, "POST", &headers(Some("x")), "field=value");
    assert!(response.starts_with("HTTP/1.1 403 "), "{response}");
}

#[test]
fn unsecured_runtime_route_cannot_bypass_secure_mutation_admission() {
    let (reads, writes, csrf, lookups, policy_calls) =
        mutation_gate(false, false, true, false, false);
    let mut legacy = route("POST");
    legacy.plan = None; // No compiler-authenticated scope contract.
    legacy.route.path = "/accounts/:resource_id".into(); // Legacy matcher syntax.
    let server = WebDevServer::bind_runtime_with_session_mutations(
        0,
        None,
        None,
        vec![legacy],
        "http://localhost",
        reads,
        writes,
    )
    .unwrap();
    let response = post(server.port, "POST", &headers(Some(&csrf)), "field=value");
    assert!(response.starts_with("HTTP/1.1 403 "), "{response}");
    assert_eq!(lookups.load(Ordering::SeqCst), 0);
    assert_eq!(policy_calls.load(Ordering::SeqCst), 0);
}

#[test]
fn unknown_session_and_bad_host_do_not_reach_mutation_authorizer() {
    let (server, csrf, lookups, policy_calls) = start(false, false, true, false, false);
    let good = headers(Some(&csrf));
    let missing = good.replace(HANDLE, "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
    let response = post(server.port, "POST", &missing, "field=value");
    assert!(response.starts_with("HTTP/1.1 401 "), "{response}");
    assert_eq!(policy_calls.load(Ordering::SeqCst), 0);

    let before = lookups.load(Ordering::SeqCst);
    let bad_host = request(server.port, &format!(
        "POST /accounts/account-7 HTTP/1.1\r\nHost: attacker.invalid\r\n{}Content-Length: 0\r\nConnection: close\r\n\r\n",
        good
    ));
    assert!(bad_host.starts_with("HTTP/1.1 403 "), "{bad_host}");
    let duplicate_host = request(server.port, &format!(
        "POST /accounts/account-7 HTTP/1.1\r\nHost: localhost\r\nHost: localhost\r\n{}Content-Length: 0\r\nConnection: close\r\n\r\n",
        good
    ));
    assert!(
        duplicate_host.starts_with("HTTP/1.1 403 "),
        "{duplicate_host}"
    );
    assert_eq!(lookups.load(Ordering::SeqCst), before);
    assert_eq!(policy_calls.load(Ordering::SeqCst), 0);
}

#[test]
fn unexpected_http_methods_are_denied_in_secure_write_mode() {
    let (server, csrf, lookups, policy_calls) = start(false, false, true, false, false);
    let response = post(server.port, "OPTIONS", &headers(Some(&csrf)), "field=value");
    assert!(response.starts_with("HTTP/1.1 403 "), "{response}");
    assert_eq!(lookups.load(Ordering::SeqCst), 0);
    assert_eq!(policy_calls.load(Ordering::SeqCst), 0);
}
