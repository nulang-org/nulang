//! Dependency-graph regression for RUSTSEC-2026-0258.
//! Any new HTTP/2 stack must use the patched h2 0.4.16+ series.

#[test]
fn resolved_lockfile_has_no_legacy_unpatched_h2() {
    let lock: toml::Value =
        toml::from_str(include_str!("../Cargo.lock")).expect("valid Cargo.lock");
    let packages = lock["package"].as_array().expect("Cargo.lock packages");
    let h2_versions: Vec<_> = packages
        .iter()
        .filter_map(|pkg| {
            (pkg["name"].as_str() == Some("h2")).then(|| pkg["version"].as_str().unwrap_or(""))
        })
        .collect();
    assert!(
        !h2_versions.is_empty(),
        "no h2 package resolved: inspect this policy before removing it"
    );
    for version in h2_versions {
        let n: Vec<u64> = version
            .split('.')
            .map(|v| v.parse().expect("numeric h2 version"))
            .collect();
        assert!(n.len() == 3 && (n[0], n[1], n[2]) >= (0, 4, 16),
            "RUSTSEC-2026-0258: vulnerable h2 {version}; migrate HTTP/2 dependencies, never add audit ignores");
    }
}

#[test]
fn vendored_libsql_disables_hyper_http2_without_removing_http1() {
    let manifest: toml::Value =
        toml::from_str(include_str!("../vendor/libsql/Cargo.toml")).expect("vendor manifest");
    let features = manifest["dependencies"]["hyper"]["features"]
        .as_array()
        .expect("upstream Hyper feature list");
    let names: Vec<_> = features
        .iter()
        .map(|value| value.as_str().unwrap())
        .collect();
    assert!(names.contains(&"http1"), "Hrana must retain HTTP/1.1");
    assert!(
        !names.contains(&"http2"),
        "legacy Hyper HTTP/2 reintroduces vulnerable h2"
    );
}
