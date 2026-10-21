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

 
// Fail closed for both language-level HTTP effects when ureq's bundled TLS
// defaults are disabled. A regression must not silently reintroduce the
// default transport instead of the explicitly OS-verified native TLS agent.
#[test]
fn vm_http_builtins_use_explicit_verified_tls_agent() {
    let vm = include_str!("../src/vm.rs");
    assert!(
        !vm.contains("ureq::get(") && !vm.contains("ureq::post("),
        "VM Http effects must not use ureq's implicit transport"
    );
    assert!(
        vm.matches("crate::registry::client::verified_https_agent()")
            .count()
            >= 2,
        "Http.get and Http.post must independently select verified native TLS"
    );
}
