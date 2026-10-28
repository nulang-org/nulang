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
    ResourceReadAuthorization, SessionLookup, SessionReadGateway, TrustedSession,
    SESSION_COOKIE_NAME,
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
fn get(port: u16, handle: &str, host: &str) -> String {
    request(port, &format!(
        "GET /accounts/account-7?tenant_id=attacker HTTP/1.1\r\nHost: {host}\r\nCookie: {SESSION_COOKIE_NAME}={handle}; tenant_id=attacker\r\nX-Tenant-Id: attacker\r\nConnection: close\r\n\r\n"
    ))
}
#[test]
fn trusted_get_executes_only_after_session_and_resource_authority() {
    let (gateway, store, policy) = gateway(false, false, true);
    let server = WebDevServer::bind_runtime_with_session_reads(
        0,
        None,
        None,
        vec![route("GET")],
        "localhost",
        gateway,
    )
    .unwrap();
    let response = get(server.port, HANDLE, "localhost");
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    assert!(response.contains("\r\n\r\ntenant-a"), "{response}");
    assert_eq!(store.load(Ordering::SeqCst), 1);
    assert_eq!(policy.load(Ordering::SeqCst), 1);
}
#[test]
fn missing_expired_wrong_tenant_or_unavailable_store_fail_closed() {
    for (expired, failed, allowed, handle, expected) in [
        (
            false,
            false,
            true,
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            401,
        ),
        (true, false, true, HANDLE, 401),
        (false, false, false, HANDLE, 403),
        (false, true, true, HANDLE, 503),
    ] {
        let (gateway, _, _) = gateway(expired, failed, allowed);
        let server = WebDevServer::bind_runtime_with_session_reads(
            0,
            None,
            None,
            vec![route("GET")],
            "localhost",
            gateway,
        )
        .unwrap();
        let response = get(server.port, handle, "localhost");
        assert!(
            response.starts_with(&format!("HTTP/1.1 {expected} ")),
            "{response}"
        );
        assert!(!response.contains("tenant-a"), "{response}");
    }
}
#[test]
fn invalid_host_and_duplicate_host_are_denied_before_session_lookup() {
    let (gateway, store, _) = gateway(false, false, true);
    let server = WebDevServer::bind_runtime_with_session_reads(
        0,
        None,
        None,
        vec![route("GET")],
        "localhost",
        gateway,
    )
    .unwrap();
    assert!(get(server.port, HANDLE, "attacker.example").starts_with("HTTP/1.1 403 "));
    let response = request(server.port, &format!(
        "GET /accounts/account-7 HTTP/1.1\r\nHost: localhost\r\nHost: localhost\r\nCookie: {SESSION_COOKIE_NAME}={HANDLE}\r\nConnection: close\r\n\r\n"
    ));
    assert!(response.starts_with("HTTP/1.1 403 "), "{response}");
    assert_eq!(store.load(Ordering::SeqCst), 0);
}
#[test]
fn mutating_requests_and_sse_remain_closed_in_read_only_mode() {
    let (gateway, store, _) = gateway(false, false, true);
    let server = WebDevServer::bind_runtime_with_session_reads(
        0,
        None,
        None,
        vec![route("POST")],
        "localhost",
        gateway,
    )
    .unwrap();
    let mutation = request(server.port, &format!(
        "POST /accounts/account-7 HTTP/1.1\r\nHost: localhost\r\nCookie: {SESSION_COOKIE_NAME}={HANDLE}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    ));
    assert!(mutation.starts_with("HTTP/1.1 403 "), "{mutation}");
    let sse = request(
        server.port,
        "GET /__nulang/sse/private HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    assert!(sse.starts_with("HTTP/1.1 403 "), "{sse}");
    assert_eq!(store.load(Ordering::SeqCst), 0);
}

#[test]
fn ordinary_dev_server_without_installed_session_gateway_remains_unauthenticated() {
    let server = WebDevServer::bind_runtime(0, None, None, vec![route("GET")]).unwrap();
    let response = get(server.port, HANDLE, "localhost");
    assert!(response.starts_with("HTTP/1.1 401 "), "{response}");
    assert!(!response.contains("tenant-a"));
}

#[test]
fn mismatched_authoritative_tenant_denies_before_resource_read_policy() {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let store_calls = Arc::new(AtomicUsize::new(0));
    let policy_calls = Arc::new(AtomicUsize::new(0));
    let store = Store {
        session: Some(TrustedSession::new(
            "principal-a",
            "tenant-other",
            now + 120,
            SessionCsrfToken::generate().unwrap(),
        )),
        fail: false,
        calls: Arc::clone(&store_calls),
    };
    let policy = Policy {
        allowed: true,
        calls: Arc::clone(&policy_calls),
    };
    let gateway = Arc::new(SessionReadGateway::new(
        Arc::new(store),
        Arc::new(policy),
        Arc::new(|_, captures: &HashMap<String, String>| {
            captures
                .get("resource_id")
                .map(|id| ("tenant-a".to_string(), id.clone()))
        }),
    ));
    let server = WebDevServer::bind_runtime_with_session_reads(
        0,
        None,
        None,
        vec![route("GET")],
        "localhost",
        gateway,
    )
    .unwrap();
    let response = get(server.port, HANDLE, "localhost");
    assert!(response.starts_with("HTTP/1.1 403 "), "{response}");
    assert_eq!(store_calls.load(Ordering::SeqCst), 1);
    assert_eq!(policy_calls.load(Ordering::SeqCst), 0);
}

#[test]
fn unauthenticated_scope_denies_before_action_envelope_validation() {
    let server = WebDevServer::bind_runtime(0, None, None, vec![route("GET")]).unwrap();
    let body = "__nulang_ui_message=%7B%22type%22%3A%22invoke_action%22%7D";
    let response = request(server.port, &format!(
        "GET /accounts/account-7 HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len(),
    ));
    assert!(response.starts_with("HTTP/1.1 401 "), "{response}");
    assert!(
        !response.contains("Invalid UI action envelope"),
        "{response}"
    );
}
