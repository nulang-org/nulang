//! Guard against silently reintroducing the unsupported rustls-webpki 0.102 branch.
#[test]
fn production_tls_dependency_tree_has_no_legacy_webpki() {
    let lock = include_str!("../Cargo.lock");
    let packages: toml::Value = lock.parse().expect("valid Cargo.lock");
    let old = packages["package"]
        .as_array()
        .unwrap()
        .iter()
        .any(|package| {
            package["name"].as_str() == Some("rustls-webpki")
                && package["version"]
                    .as_str()
                    .unwrap_or("")
                    .starts_with("0.102.")
        });
    assert!(
        !old,
        "RUSTSEC-2026-0382: legacy rustls-webpki 0.102 remains in Cargo.lock"
    );
}
