//! Lockfile policy regression for the audited TLS owner-migration candidate.
//!
//! The configured cargo-audit gate alone cannot prove that obsolete advisory
//! exceptions have been removed. This contract checks both the resolved
//! dependency graph and the committed audit policy, without allowing advisory
//! suppression to masquerade as a patched dependency.

#[test]
fn legacy_tls_crates_and_obsolete_security_exemptions_are_absent() {
    let lock = include_str!("../Cargo.lock");
    let audit_policy = include_str!("../.cargo/audit.toml");

    for (name, version_prefix) in [
        ("rustls", "0.22."),
        ("rustls-webpki", "0.102."),
        ("webpki-roots", "0.26."),
        ("hyper-rustls", "0.25."),
    ] {
        let package_prefix = format!("name = \"{name}\"\nversion = \"{version_prefix}");
        assert!(
            !lock.contains(&package_prefix),
            "legacy TLS package {name} {version_prefix} must not re-enter the lockfile"
        );
    }

    for advisory in [
        "RUSTSEC-2026-0346",
        "RUSTSEC-2026-0349",
        "RUSTSEC-2026-0049",
        "RUSTSEC-2026-0098",
        "RUSTSEC-2026-0099",
        "RUSTSEC-2026-0104",
    ] {
        assert!(
            !audit_policy.contains(advisory),
            "obsolete RustSec exemption {advisory} must not hide a regression"
        );
    }
}
