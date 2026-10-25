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

// Ordinary language-level HTTP effects must select their own explicit agent:
// they may access HTTP services, but HTTPS still uses OS-verified native TLS.
// Credential-bearing registry/deploy operations have a stricter admission
// policy and must never silently switch to the VM's more permissive agent.
#[test]
fn vm_http_builtins_use_explicit_native_tls_agent() {
    let vm = include_str!("../src/vm.rs");
    assert!(
        !vm.contains("ureq::get(") && !vm.contains("ureq::post("),
        "VM Http effects must not use ureq's implicit transport"
    );
    assert_eq!(
        vm.matches("crate::registry::client::vm_effect_http_agent(&url)")
            .count(),
        2,
        "Http.get and Http.post must each use the explicit VM HTTP agent"
    );
    assert!(
        !vm.contains("crate::registry::client::verified_https_agent(&url)"),
        "VM Http effects must not inherit registry/deploy bearer restrictions"
    );

    let registry = include_str!("../src/registry/client.rs");
    let vm_agent = registry
        .split_once("pub(crate) fn vm_effect_http_agent")
        .expect("VM HTTP agent must exist")
        .1
        .split_once("/// Client for a package registry server.")
        .expect("VM agent must be separate from registry client")
        .0;
    assert!(
        vm_agent.contains("native_tls::TlsConnector::new()")
            && vm_agent.contains(".tls_connector(")
            && vm_agent.contains(".https_only(https_only)"),
        "VM HTTP agent must use native OS trust and forbid HTTPS downgrade"
    );
}

#[test]
fn disposable_turso_certification_proves_remote_write_and_reconnect() {
    let example = include_str!("../examples/libsql_live_turso_certification.rs");
    assert!(
        example.contains("NULANG_TURSO_TEST_IS_DISPOSABLE"),
        "live certification must remain explicitly opt-in"
    );
    assert!(
        example.contains("write_certification_probe(&mut store, actor_id)"),
        "certification must write an isolated durable probe"
    );
    assert!(
        example.contains("drop(store)")
            && example.contains("LibsqlStore::new_remote(&endpoint, &token)")
            && example.contains("verify_certification_probe(&reader, actor_id)"),
        "certification must reopen authenticated remote SQL to verify durable readback"
    );
}

#[test]
fn registry_bearer_requests_keep_strict_transport_admission() {
    let registry = include_str!("../src/registry/client.rs");
    let privileged_client = registry
        .split_once("/// Client for a package registry server.")
        .expect("registry client must exist")
        .1;
    assert!(
        privileged_client.contains("let agent = verified_https_agent(&url)?;")
            && privileged_client.contains("verified_https_agent(&url)?.get(&url)"),
        "registry operations must use credential-restricted HTTPS admission"
    );
    assert!(
        !privileged_client.contains("vm_effect_http_agent("),
        "registry/deploy traffic must not use the permissive VM HTTP agent"
    );
    assert!(
        registry.contains("plaintext registry/deploy URLs require an exact loopback host"),
        "credential-bearing plaintext must remain restricted to literal loopback hosts"
    );
}
