//! Locks down the small self-hosted server feature closure.
//!
//! The package must be buildable with `--no-default-features --features server`
//! without pulling the compiler JIT, Python, AI providers, editor tooling, or
//! external storage backends. The CI smoke additionally audits `cargo tree`.

use std::collections::BTreeSet;

fn features() -> toml::value::Table {
    let manifest: toml::Value = include_str!("../Cargo.toml")
        .parse()
        .expect("workspace manifest should parse");
    manifest["features"]
        .as_table()
        .expect("Cargo features should be a table")
        .clone()
}

fn visit_feature(
    name: &str,
    features: &toml::value::Table,
    visited: &mut BTreeSet<String>,
) {
    if !visited.insert(name.to_owned()) {
        return;
    }
    let entries = features
        .get(name)
        .unwrap_or_else(|| panic!("feature {name} must exist"))
        .as_array()
        .unwrap_or_else(|| panic!("feature {name} should contain an array"));
    for entry in entries {
        let dependency = entry.as_str().expect("feature entry must be a string");
        // Cargo's dep:xxx and package/feature entries are not feature aliases.
        if !dependency.starts_with("dep:") && !dependency.contains('/') {
            visit_feature(dependency, features, visited);
        }
    }
}

#[test]
fn server_profile_includes_only_required_runtime_capabilities() {
    let features = features();
    let mut enabled = BTreeSet::new();
    visit_feature("server", &features, &mut enabled);

    for required in ["sqlite", "tls", "tcp", "ureq"] {
        assert!(enabled.contains(required), "server must include {required}");
    }

    for forbidden in [
        "full",
        "release-cli",
        "native-codegen",
        "python",
        "lsp",
        "ai-runtime",
        "ffi",
        "wasm-backend",
        "wasmfx-backend",
        "rocksdb",
        "postgres",
        "cache-server",
        "otel",
        "difffuzz",
    ] {
        assert!(
            !enabled.contains(forbidden),
            "server must not enable optional subsystem {forbidden}"
        );
    }
}

#[test]
fn server_profile_does_not_change_full_default_compatibility() {
    let features = features();
    let defaults = features["default"]
        .as_array()
        .expect("default features must be a list");
    assert_eq!(defaults.len(), 1);
    assert_eq!(defaults[0].as_str(), Some("full"));
}
