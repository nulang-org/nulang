//! Regression guard: the modern HTTP/2 client graph must not resolve an
//! affected h2 0.4 release (RUSTSEC-2026-0258).
//!
//! IMPORTANT: this does NOT remediate legacy h2 0.3 via libsql / Hyper 0.14.
//! The project must remove or migrate that legacy transport separately.

#[test]
fn modern_http2_dependency_is_patched_against_empty_data_frame_dos() {
    let lockfile: toml::Value =
        toml::from_str(include_str!("../Cargo.lock")).expect("parse the committed Cargo.lock");
    let packages = lockfile["package"].as_array().expect("Cargo.lock packages");
    let modern_h2: Vec<&str> = packages
        .iter()
        .filter(|pkg| pkg["name"].as_str() == Some("h2"))
        .filter_map(|pkg| pkg["version"].as_str())
        .filter(|version| version.starts_with("0.4."))
        .collect();
    assert!(
        !modern_h2.is_empty(),
        "expected modern h2 transport to be locked"
    );
    for version in modern_h2 {
        let patch: u64 = version
            .trim_start_matches("0.4.")
            .parse()
            .expect("valid h2 patch");
        assert!(patch >= 16, "h2 {version} vulnerable to RUSTSEC-2026-0258");
    }
}
