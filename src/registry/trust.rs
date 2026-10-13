//! Publisher-authorized registry archive verification.
//!
//! Trust roots are explicit, project-controlled Ed25519 public keys in
//! `Nulang.trust.toml`, NEVER public keys obtained from the registry server.

#[cfg(test)]
mod tests {
    use super::*;

    const REGISTRY: &str = "https://registry.example";
    const NAME: &str = "util";
    const VERSION: &str = "0.1.0";

    fn seed() -> [u8; 32] {
        [7u8; 32]
    }

    fn signed_fixture() -> (Vec<u8>, String, String) {
        let archive = b"test tarball bytes".to_vec();
        let seed = seed();
        let signature = sign_registry_archive(REGISTRY, NAME, VERSION, &archive, &seed);
        let key = signing_public_key(&seed).to_string();
        (archive, signature, key)
    }

    #[test]
    fn publisher_signature_verifies_only_for_explicitly_pinned_key() {
        let (archive, signature, key) = signed_fixture();
        verify_publisher_signature(
            REGISTRY,
            NAME,
            VERSION,
            &archive,
            &signature,
            &[key.clone()],
        ).expect("valid signed archive under pinned publisher key");
        assert!(
            verify_publisher_signature(REGISTRY, NAME, VERSION, &archive, &signature, &[]).is_err(),
            "no trusted keys must fail closed"
        );
        assert!(
            verify_publisher_signature(
                REGISTRY, NAME, VERSION, &archive, &signature, &["00".repeat(32)]
            ).is_err(),
            "untrusted publisher key cannot authenticate a package"
        );
    }

    #[test]
    fn publisher_signature_binds_registry_package_version_and_exact_bytes() {
        let (archive, signature, key) = signed_fixture();
        for (registry, name, version, data) in [
            ("https://evil.example", NAME, VERSION, archive.as_slice()),
            (REGISTRY, "other", VERSION, archive.as_slice()),
            (REGISTRY, NAME, "0.2.0", archive.as_slice()),
            (REGISTRY, NAME, VERSION, b"mutated tarball bytes".as_slice()),
        ] {
            assert!(
                verify_publisher_signature(registry, name, version, data, &signature, &[key.clone()])
                    .is_err(),
                "publisher signature must bind all registry package coordinates and archive contents"
            );
        }
    }

    #[test]
    fn publisher_signature_rejects_truncated_and_invalid_hex() {
        let (archive, signature, key) = signed_fixture();
        for invalid in ["", "bogus", &signature[..signature.len() - 2]] {
            assert!(
                verify_publisher_signature(
                    REGISTRY, NAME, VERSION, &archive, invalid, &[key.clone()]
                ).is_err()
            );
        }
    }

    #[test]
    fn publisher_trust_store_requires_pinned_keys_without_remote_fallback() {
        let root = std::env::temp_dir().join(format!(
            "nula_registry_trust_{}", std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        assert!(trusted_publisher_keys(&root, REGISTRY).is_err());

        let (_, _, key) = signed_fixture();
        let trust = format!(
            "[registries.\"https://registry.example\"]\npublic_keys = [\"{}\"]\n",
            key
        );
        std::fs::write(root.join("Nulang.trust.toml"), trust).unwrap();
        assert_eq!(trusted_publisher_keys(&root, REGISTRY).unwrap(), vec![key]);
        assert!(trusted_publisher_keys(&root, "https://unknown.example").is_err());
        let _ = std::fs::remove_dir_all(&root);
    }
}
