//! Real HTTP boundary regression for the experimental tenant-resource example.
//! This is not a production session or persistence adapter.
use crate::runtime::http_server::{kv_delete, kv_get, kv_set};
use crate::runtime::{HttpMethod, WebDevServer, WebRoute};
use crate::{lexer::Lexer, parser::Parser, typechecker::TypeChecker};
use rand_core::{OsRng, RngCore};
use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream};
use std::time::Duration;

fn compile_routes() -> Vec<WebRoute> {
    let source = include_str!("../../examples/tenant-resources/src/http_contract.nula");
    let ast = Parser::new(Lexer::new(source).lex().unwrap())
        .parse_module()
        .unwrap();
    let mut tc = TypeChecker::new();
    tc.check_module(&ast)
        .expect("application HTTP policy typechecks");
    let hir = crate::hir_lower::lower_module(&ast, &tc.inferred_decl_types);
    let mut mir = crate::mir_lower::lower_module(&hir).unwrap();
    let module = crate::mir_codegen::compile_mir(&mut mir, "tenant-http").unwrap();
    [
        (HttpMethod::Get, "/faces/:id", "read_face"),
        (HttpMethod::Post, "/faces/:id", "write_face"),
        (HttpMethod::Get, "/campaigns/:id", "read_campaign"),
        (HttpMethod::Post, "/campaigns/:id", "write_campaign"),
    ]
    .iter()
    .map(|(method, path, name)| {
        let debug = module
            .debug_functions
            .iter()
            .find(|d| d.name == *name)
            .unwrap();
        let function_index = module
            .function_table
            .iter()
            .position(|o| *o == debug.code_offset)
            .unwrap();
        WebRoute {
            method: *method,
            path: path.to_string(),
            handler_module: module.clone(),
            handler_func_idx: function_index,
        }
    })
    .collect()
}

fn send(
    port: u16,
    method: &str,
    path: &str,
    cookie: Option<&str>,
    csrf: Option<&str>,
    body: &str,
) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut req=format!("{method} {path} HTTP/1.1\r\nHost: localhost:{port}\r\nConnection: close\r\nContent-Length: {}\r\n",body.len());
    if let Some(v) = cookie {
        req.push_str(&format!("Cookie: demo_session={v}\r\n"));
    }
    if let Some(v) = csrf {
        req.push_str(&format!("X-CSRF-Token: {v}\r\n"));
    }
    if !body.is_empty() {
        req.push_str("Content-Type: application/x-www-form-urlencoded\r\n");
    }
    req.push_str("\r\n");
    req.push_str(body);
    stream.write_all(req.as_bytes()).unwrap();
    stream.shutdown(Shutdown::Write).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    response
}

struct Fixture {
    keys: Vec<String>,
}
impl Fixture {
    fn new() -> Self {
        Self { keys: vec![] }
    }
    fn put(&mut self, k: &str, v: &str) {
        kv_set(k, v);
        self.keys.push(k.to_string());
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        for k in &self.keys {
            kv_delete(k);
        }
    }
}

fn random_secret() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn authenticated_http_read_and_mutation_enforce_tenant_and_permissions() {
    let routes = compile_routes();
    let server = WebDevServer::bind(0, None, None, routes).unwrap();
    let token_a = random_secret();
    let token_b = random_secret();
    let token_viewer = random_secret();
    let csrf_a = random_secret();
    let csrf_b = random_secret();
    let csrf_viewer = random_secret();
    let mut fixture = Fixture::new();
    fixture.put(&format!("tenant-demo:session:{token_a}"), "tenant-a:editor");
    fixture.put(&format!("tenant-demo:session:{token_b}"), "tenant-b:editor");
    fixture.put(
        &format!("tenant-demo:session:{token_viewer}"),
        "tenant-a:viewer",
    );
    fixture.put(&format!("tenant-demo:csrf:{token_a}"), &csrf_a);
    fixture.put(&format!("tenant-demo:csrf:{token_b}"), &csrf_b);
    fixture.put(&format!("tenant-demo:csrf:{token_viewer}"), &csrf_viewer);
    fixture.put(&format!("tenant-demo:csrf-live:{token_a}"), "active");
    fixture.put(&format!("tenant-demo:csrf-live:{token_b}"), "active");
    fixture.put(&format!("tenant-demo:csrf-live:{token_viewer}"), "active");
    fixture.put("tenant-demo:face:tenant-a:face-a", "Existing A");
    fixture.put("tenant-demo:face:tenant-b:face-b", "Existing B");
    fixture.put("tenant-demo:face:tenant-a:shared", "Shared A");
    fixture.put("tenant-demo:face:tenant-b:shared", "Shared B");
    for (tenant, id) in [
        ("tenant-a", "face-a"),
        ("tenant-b", "face-b"),
        ("tenant-a", "shared"),
        ("tenant-b", "shared"),
    ] {
        fixture.put(&format!("tenant-demo:face-live:{tenant}:{id}"), "active");
    }
    fixture.put(
        "tenant-demo:campaign:tenant-a:campaign-a",
        "Original campaign A",
    );
    fixture.put("tenant-demo:campaign-face:tenant-a:campaign-a", "face-a");
    fixture.put(
        "tenant-demo:campaign:tenant-b:campaign-b",
        "Original campaign B",
    );
    fixture.put("tenant-demo:campaign-face:tenant-b:campaign-b", "face-b");
    fixture.put("tenant-demo:campaign-live:tenant-a:campaign-a", "active");
    fixture.put("tenant-demo:campaign-live:tenant-b:campaign-b", "active");
    let same_id_a = send(
        server.port,
        "GET",
        "/faces/shared",
        Some(&token_a),
        None,
        "",
    );
    assert!(
        same_id_a.contains("Shared A"),
        "tenant A read must select its own duplicate ID: {same_id_a}"
    );
    assert!(
        !same_id_a.contains("Shared B"),
        "tenant A saw tenant B duplicate ID: {same_id_a}"
    );
    let same_id_b = send(
        server.port,
        "GET",
        "/faces/shared",
        Some(&token_b),
        None,
        "",
    );
    assert!(
        same_id_b.contains("Shared B"),
        "tenant B read must select its own duplicate ID: {same_id_b}"
    );
    assert!(
        !same_id_b.contains("Shared A"),
        "tenant B saw tenant A duplicate ID: {same_id_b}"
    );
    let update_shared = send(
        server.port,
        "POST",
        "/faces/shared",
        Some(&token_a),
        Some(&csrf_a),
        "name=Shared%20updated",
    );
    assert!(
        update_shared.contains("UPDATED"),
        "tenant A shared-resource update failed: {update_shared}"
    );
    assert_eq!(
        kv_get("tenant-demo:face:tenant-a:shared").as_deref(),
        Some("Shared updated")
    );
    assert_eq!(
        kv_get("tenant-demo:face:tenant-b:shared").as_deref(),
        Some("Shared B")
    );
    let ok = send(
        server.port,
        "GET",
        "/faces/face-a",
        Some(&token_a),
        None,
        "",
    );
    assert!(ok.contains("Existing A"), "{ok}");
    let denied = send(
        server.port,
        "GET",
        "/faces/face-a",
        Some(&token_b),
        None,
        "",
    );
    assert!(
        !denied.contains("Existing A"),
        "cross tenant data leaked: {denied}"
    );
    let anonymous = send(server.port, "GET", "/faces/face-a", None, None, "");
    assert!(
        !anonymous.contains("Existing A"),
        "anonymous data leaked: {anonymous}"
    );
    let forged = send(
        server.port,
        "GET",
        "/faces/face-a",
        Some("forged-token"),
        None,
        "",
    );
    assert!(
        !forged.contains("Existing A"),
        "forged session leaked: {forged}"
    );
    let viewer_read = send(
        server.port,
        "GET",
        "/faces/face-a",
        Some(&token_viewer),
        None,
        "",
    );
    assert!(
        viewer_read.contains("Existing A"),
        "read-only role cannot read: {viewer_read}"
    );
    let viewer_write = send(
        server.port,
        "POST",
        "/faces/face-a",
        Some(&token_viewer),
        Some(&csrf_viewer),
        "name=Viewer%20write",
    );
    assert!(
        !viewer_write.contains("UPDATED"),
        "read-only role modified face: {viewer_write}"
    );
    let bad_write = send(
        server.port,
        "POST",
        "/faces/face-a",
        Some(&token_b),
        Some(&csrf_b),
        "name=Stolen",
    );
    assert!(
        !bad_write.contains("UPDATED"),
        "cross-tenant write succeeded: {bad_write}"
    );
    let wrong_csrf = send(
        server.port,
        "POST",
        "/faces/face-a",
        Some(&token_a),
        None,
        "name=Stolen",
    );
    assert!(
        !wrong_csrf.contains("UPDATED"),
        "missing token write succeeded: {wrong_csrf}"
    );
    let missing_name = send(
        server.port,
        "POST",
        "/faces/face-a",
        Some(&token_a),
        Some(&csrf_a),
        "tenant_id=tenant-b",
    );
    assert!(
        !missing_name.contains("UPDATED"),
        "missing name accepted: {missing_name}"
    );
    let duplicate_name = send(
        server.port,
        "POST",
        "/faces/face-a",
        Some(&token_a),
        Some(&csrf_a),
        "name=Good&name=Evil",
    );
    assert!(
        !duplicate_name.contains("UPDATED"),
        "duplicate name accepted: {duplicate_name}"
    );
    assert_eq!(
        kv_get("tenant-demo:face:tenant-a:face-a").as_deref(),
        Some("Existing A")
    );
    let wrote = send(
        server.port,
        "POST",
        "/faces/face-a",
        Some(&token_a),
        Some(&csrf_a),
        "name=Updated%20A",
    );
    assert!(
        wrote.contains("UPDATED"),
        "same-tenant mutation failed: {wrote}"
    );
    assert_eq!(
        kv_get("tenant-demo:face:tenant-a:face-a").as_deref(),
        Some("Updated A")
    );
    let verify = send(
        server.port,
        "GET",
        "/faces/face-a",
        Some(&token_a),
        None,
        "",
    );
    assert!(
        verify.contains("Updated A"),
        "cross-request update not visible: {verify}"
    );
    let campaign_ok = send(
        server.port,
        "GET",
        "/campaigns/campaign-a",
        Some(&token_a),
        None,
        "",
    );
    assert!(campaign_ok.contains("Original campaign A"), "{campaign_ok}");
    let campaign_foreign = send(
        server.port,
        "GET",
        "/campaigns/campaign-a",
        Some(&token_b),
        None,
        "",
    );
    assert!(
        !campaign_foreign.contains("Original campaign A"),
        "cross-tenant campaign read: {campaign_foreign}"
    );
    let campaign_missing = send(server.port, "GET", "/campaigns/campaign-a", None, None, "");
    assert!(
        !campaign_missing.contains("Original campaign A"),
        "anonymous campaign read: {campaign_missing}"
    );
    let viewer_campaign_read = send(
        server.port,
        "GET",
        "/campaigns/campaign-a",
        Some(&token_viewer),
        None,
        "",
    );
    assert!(
        viewer_campaign_read.contains("Original campaign A"),
        "read-only campaign read denied: {viewer_campaign_read}"
    );
    let viewer_campaign_write = send(
        server.port,
        "POST",
        "/campaigns/campaign-a",
        Some(&token_viewer),
        Some(&csrf_viewer),
        "title=Viewer%20write",
    );
    assert!(
        !viewer_campaign_write.contains("UPDATED"),
        "read-only role modified campaign: {viewer_campaign_write}"
    );
    let bad_campaign_write = send(
        server.port,
        "POST",
        "/campaigns/campaign-a",
        Some(&token_b),
        Some(&csrf_b),
        "title=Stolen",
    );
    assert!(
        !bad_campaign_write.contains("UPDATED"),
        "cross-tenant campaign write: {bad_campaign_write}"
    );
    let missing_title = send(
        server.port,
        "POST",
        "/campaigns/campaign-a",
        Some(&token_a),
        Some(&csrf_a),
        "tenant_id=tenant-b",
    );
    assert!(
        !missing_title.contains("UPDATED"),
        "missing campaign title accepted: {missing_title}"
    );
    let duplicate_title = send(
        server.port,
        "POST",
        "/campaigns/campaign-a",
        Some(&token_a),
        Some(&csrf_a),
        "title=Good&title=Evil",
    );
    assert!(
        !duplicate_title.contains("UPDATED"),
        "duplicate campaign title accepted: {duplicate_title}"
    );
    let campaign_write = send(
        server.port,
        "POST",
        "/campaigns/campaign-a",
        Some(&token_a),
        Some(&csrf_a),
        "title=Campaign%20updated",
    );
    assert!(
        campaign_write.contains("UPDATED"),
        "same-tenant campaign update failed: {campaign_write}"
    );
    assert_eq!(
        kv_get("tenant-demo:campaign:tenant-a:campaign-a").as_deref(),
        Some("Campaign updated")
    );
    fixture.put("tenant-demo:campaign-face:tenant-a:campaign-a", "face-b");
    let invalid_join = send(
        server.port,
        "GET",
        "/campaigns/campaign-a",
        Some(&token_a),
        None,
        "",
    );
    assert!(
        !invalid_join.contains("Campaign updated"),
        "cross-tenant join leaked: {invalid_join}"
    );
    let invalid_update = send(
        server.port,
        "POST",
        "/campaigns/campaign-a",
        Some(&token_a),
        Some(&csrf_a),
        "title=Bad%20link",
    );
    assert!(
        !invalid_update.contains("UPDATED"),
        "invalid relation update allowed: {invalid_update}"
    );
    assert_eq!(
        kv_get("tenant-demo:campaign:tenant-a:campaign-a").as_deref(),
        Some("Campaign updated")
    );
}
