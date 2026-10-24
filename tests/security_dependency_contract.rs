// Guards the exact locked dependency graph against reintroducing known
// October 2026 vulnerabilities, not just nominal Cargo.toml version bounds.
#[test]
fn patched_transport_dependencies_exclude_vulnerable_locked_versions() {
    let lock = include_str!("../Cargo.lock");
    for (name, version) in [
        ("libsql", "0.10.0-pre.4"),
        ("wasmtime", "48.0.6"),
        ("rustls-webpki", "0.102.8"),
        ("rustls-webpki", "0.103.14"),
        ("webpki-roots", "0.26.11"),
        ("webpki-roots", "1.0.9"),
    ] {
        let forbidden = format!(
            "[[package]]\nname = \"{}\"\nversion = \"{}\"",
            name, version
        );
        assert!(
            !lock.contains(&forbidden),
            "outdated {name} {version} reintroduced"
        );
    }
}

// Generic VM HTTP and credential-bearing registry requests have different
// admission policies, but both must explicitly use verified OS-native TLS.
// VM HTTP supports ordinary localhost/private HTTP without a bearer token;
// the registry client restricts plaintext to numeric loopback and prevents
// redirecting credential-bearing plaintext requests off-origin.
#[test]
fn generic_vm_and_registry_http_keep_distinct_verified_tls_policies() {
    let vm = include_str!("../src/vm.rs");
    let client = include_str!("../src/registry/client.rs");
    assert!(
        !vm.contains("ureq::get(") && !vm.contains("ureq::post("),
        "VM Http effects must not use ureq's implicit transport"
    );
    assert_eq!(
        vm.matches("crate::registry::client::vm_effect_http_agent(&url)")
            .count(),
        2,
        "both Http.get and Http.post must select the explicit generic VM agent"
    );
    assert!(
        !vm.contains("crate::registry::client::verified_https_agent(&url)"),
        "generic VM effects must not inherit credential-bearing registry admission"
    );

    let generic_agent = client
        .split("pub(crate) fn vm_effect_http_agent(")
        .nth(1)
        .expect("generic VM transport must be declared")
        .split("/// Client for a package registry server.")
        .next()
        .unwrap();
    assert!(
        generic_agent.contains("native_tls::TlsConnector::new()")
            && generic_agent.contains(".tls_connector(")
            && generic_agent.contains(".https_only(https_only)"),
        "generic HTTP must explicitly verify native TLS and fence HTTPS downgrades"
    );

    let bearer_agent = client
        .split("pub(crate) fn verified_https_agent(")
        .nth(1)
        .expect("registry/deploy verified agent must remain")
        .split("/// HTTP transport for VM effects")
        .next()
        .unwrap();
    assert!(
        bearer_agent.contains("native_tls::TlsConnector::new()")
            && bearer_agent.contains(".tls_connector(")
            && bearer_agent.contains(".https_only(https_only)")
            && bearer_agent.contains(".redirects(if https_only { 5 } else { 0 })"),
        "registry/deploy transport must verify TLS and forbid plaintext bearer redirects"
    );
    assert!(
        client.matches("verified_https_agent(&url)?").count() >= 3,
        "registry publish/fetch/list must retain bearer-aware transport admission"
    );
}
