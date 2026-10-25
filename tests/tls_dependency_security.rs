//! Guard against the unsupported TLS dependency chain used by older libSQL.
//! This inspects Cargo's canonical package records, without a separate TOML
//! parser that may not support the current Cargo.lock format.

fn locked_version_has_prefix(lock: &str, crate_name: &str, prefix: &str) -> bool {
    lock.split("[[package]]").skip(1).any(|package| {
        let mut name = None;
        let mut version = None;
        for line in package.lines() {
            if let Some(value) = line.trim().strip_prefix("name = \"") {
                name = value.strip_suffix('"');
            } else if let Some(value) = line.trim().strip_prefix("version = \"") {
                version = value.strip_suffix('"');
            }
        }
        name == Some(crate_name) && version.is_some_and(|v| v.starts_with(prefix))
    })
}

#[test]
fn production_tls_dependency_tree_has_no_legacy_webpki() {
    let lock = include_str!("../Cargo.lock");
    for (name, prefix) in [
        ("rustls-webpki", "0.102."),
        ("rustls", "0.22."),
        ("hyper-rustls", "0.25."),
    ] {
        assert!(
            !locked_version_has_prefix(lock, name, prefix),
            "obsolete TLS dependency remains in Cargo.lock: {name} {prefix}*"
        );
    }
    assert!(locked_version_has_prefix(lock, "rustls-webpki", "0.103.16"));
}
