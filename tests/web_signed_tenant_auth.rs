use ed25519_dalek::{Signer, SigningKey};
use nulang::runtime::{HttpMethod, HttpRequest, SignedTenantAuthorizer};
use std::sync::Arc;

fn request(auth_values: &[&str]) -> HttpRequest {
    HttpRequest {
        method: HttpMethod::Get,
        path: "/tenants/acme/inventory".into(),
        headers: auth_values
            .iter()
            .map(|value| ("Authorization".into(), (*value).into()))
            .collect(),
        body: vec![],
    }
}

fn token(signing: &SigningKey, claims: serde_json::Value) -> String {
    let payload = serde_json::to_vec(&claims).unwrap();
    let mut message = b"nulang-web-capability-v1\0".to_vec();
    message.extend_from_slice(&payload);
    let signature = signing.sign(&message);
    format!(
        "nula1.{}.{}",
        hex::encode(&payload),
        hex::encode(signature.to_bytes())
    )
}

#[test]
fn signed_tenant_token_requires_valid_signature_tenant_audience_time_and_permission() {
    let signing = SigningKey::from_bytes(&[42u8; 32]);
    let tenant = Arc::new(|request: &HttpRequest| {
        request
            .path
            .strip_prefix("/tenants/")
            .and_then(|remaining| remaining.split('/').next())
            .map(str::to_string)
    });
    let verifier =
        SignedTenantAuthorizer::new(signing.verifying_key(), "nulang-web-test", tenant).unwrap();
    let claims = serde_json::json!({
        "sub": "user-42", "tenant": "acme", "aud": "nulang-web-test",
        "iat": 1000, "exp": 1100, "permissions": ["inventory.read"]
    });
    let good_token = token(&signing, claims.clone());
    assert!(verifier.verify_at(
        &request(&[&format!("Bearer {good_token}")]),
        "inventory.read",
        1050
    ));
    assert!(!verifier.verify_at(
        &request(&[&format!("Bearer {good_token}")]),
        "inventory.write",
        1050
    ));
    assert!(!verifier.verify_at(
        &request(&[&format!("Bearer {good_token}")]),
        "inventory.read",
        1100
    ));
    assert!(!verifier.verify_at(
        &request(&[&format!("Bearer {good_token}")]),
        "inventory.read",
        999
    ));
    assert!(!verifier.verify_at(
        &request(&[
            &format!("Bearer {good_token}"),
            &format!("Bearer {good_token}")
        ]),
        "inventory.read",
        1050
    ));
    let forged = format!("Bearer {}a", good_token);
    assert!(!verifier.verify_at(&request(&[&forged]), "inventory.read", 1050));
    for (field, value) in [("tenant", "other"), ("aud", "another-service"), ("sub", "")] {
        let mut changed = claims.clone();
        changed[field] = serde_json::json!(value);
        let signed = token(&signing, changed);
        assert!(!verifier.verify_at(
            &request(&[&format!("Bearer {signed}")]),
            "inventory.read",
            1050
        ));
    }
}

#[test]
fn signed_tenant_token_rejects_untrusted_header_and_excessive_lifetime() {
    let signing = SigningKey::from_bytes(&[13u8; 32]);
    let verifier = SignedTenantAuthorizer::new(
        signing.verifying_key(),
        "web",
        Arc::new(|_: &HttpRequest| Some("acme".into())),
    )
    .unwrap();
    let long_lived = token(
        &signing,
        serde_json::json!({
            "sub":"alice", "tenant":"acme", "aud":"web", "iat":100, "exp":100_000,
            "permissions":["inventory.read"]
        }),
    );
    assert!(!verifier.verify_at(
        &request(&[&format!("Bearer {long_lived}")]),
        "inventory.read",
        150
    ));
    assert!(!verifier.verify_at(
        &HttpRequest {
            method: HttpMethod::Get,
            path: "/tenants/acme/inventory".into(),
            headers: vec![("X-Nulang-Capabilities".into(), "inventory.read".into())],
            body: vec![]
        },
        "inventory.read",
        150
    ));
}

fn http_request_for_sse(port: u16, path: &str, credential: &str, streaming: bool) -> String {
    use std::io::{Read, Write};
    use std::net::{Shutdown, TcpStream};
    use std::time::Duration;
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    write!(stream, "GET {path} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {credential}\r\nConnection: close\r\n\r\n").unwrap();
    if streaming {
        let mut buf = [0u8; 1024];
        let n = stream.read(&mut buf).unwrap();
        String::from_utf8_lossy(&buf[..n]).to_string()
    } else {
        stream.shutdown(Shutdown::Write).unwrap();
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).unwrap();
        String::from_utf8(buf).unwrap()
    }
}

#[test]
fn signed_tenant_bearer_authenticates_exact_sse_room_and_denies_cross_tenant() {
    use nulang::runtime::WebDevServer;
    use std::time::{SystemTime, UNIX_EPOCH};
    let signing = SigningKey::from_bytes(&[72u8; 32]);
    let tenant_resolver = Arc::new(|request: &HttpRequest| {
        request
            .path
            .strip_prefix("/__nulang/sse/tenant-")
            .map(str::to_string)
    });
    let verifier =
        SignedTenantAuthorizer::new(signing.verifying_key(), "web-sse", tenant_resolver).unwrap();
    let server = WebDevServer::bind_runtime_with_authorizer(
        0,
        None,
        None,
        Vec::new(),
        verifier.into_authorizer(),
    )
    .unwrap();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let claims = serde_json::json!({"sub":"user-42", "tenant":"acme", "aud":"web-sse",
        "iat":now.saturating_sub(1), "exp":now+60,
        "permissions":["sse.subscribe.tenant-acme"]});
    let signed = token(&signing, claims);
    let authorized = http_request_for_sse(server.port, "/__nulang/sse/tenant-acme", &signed, true);
    assert!(
        authorized.starts_with("HTTP/1.1 200 OK\r\n"),
        "{authorized}"
    );
    assert!(authorized.contains("text/event-stream"), "{authorized}");
    let other_tenant =
        http_request_for_sse(server.port, "/__nulang/sse/tenant-other", &signed, false);
    assert!(
        other_tenant.starts_with("HTTP/1.1 403 Forbidden\r\n"),
        "{other_tenant}"
    );
}
