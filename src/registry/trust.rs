//! Explicit publisher trust for Nula package registry archives.
//!
//! A registry server transports an Ed25519 signature; the client accepts it
//! ONLY when it verifies against a public key pinned independently by the
//! consuming project in Nulang.trust.toml. Neither the server nor the package
//! itself may choose the trusted keys.

use std::collections::BTreeMap;
use std::path::Path;

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::Deserialize;

pub const TRUST_FILE: &str = "Nulang.trust.toml";
pub const SIGNATURE_HEADER: &str = "X-Nula-Signature";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TrustConfig {
    registries: BTreeMap<String, TrustedRegistry>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TrustedRegistry {
    public_keys: Vec<String>,
}

/// Return an exact registry URL's explicitly configured publisher keys.
/// There is no implicit trust-on-first-use and no fallback to server-provided
/// public keys. A project author must review/pin trusted public keys.
pub fn trusted_publisher_keys(root: &Path, registry_url: &str) -> Result<Vec<String>, String> {
    let path = root.join(TRUST_FILE);
    let source = std::fs::read_to_string(&path).map_err(|err| {
        format!(
            "registry publisher trust requires an explicit {}: {}",
            path.display(), err
        )
    })?;
    let config: TrustConfig = toml::from_str(&source).map_err(|err| {
        format!("invalid {}: {}", path.display(), err)
    })?;
    let url = registry_url.trim_end_matches('/');
    let keys = config.registries.get(url).ok_or_else(|| {
        format!("no pinned publisher keys for registry '{}' in {}", url, path.display())
    })?;
    if keys.public_keys.is_empty() {
        return Err(format!("registry '{}' has no trusted publisher keys", url));
    }
    for key in &keys.public_keys {
        parse_verifying_key(key)?;
    }
    Ok(keys.public_keys.clone())
}

fn parse_verifying_key(hex_key: &str) -> Result<VerifyingKey, String> {
    let data = hex::decode(hex_key).map_err(|_| "publisher public key must be hex".to_string())?;
    let key: [u8; 32] = data.try_into().map_err(|_| {
        "publisher public key must be exactly 32 bytes".to_string()
    })?;
    VerifyingKey::from_bytes(&key).map_err(|err| format!("invalid Ed25519 publisher key: {}", err))
}

fn signed_coordinates(registry_url: &str, name: &str, version: &str, archive: &[u8]) -> Vec<u8> {
    fn field(message: &mut Vec<u8>, value: &[u8]) {
        message.extend_from_slice(&(value.len() as u32).to_be_bytes());
        message.extend_from_slice(value);
    }
    let mut message = b"nulang-registry-publisher-v1\0".to_vec();
    field(&mut message, registry_url.trim_end_matches('/').as_bytes());
    field(&mut message, name.as_bytes());
    field(&mut message, version.as_bytes());
    message.extend_from_slice(blake3::hash(archive).as_bytes());
    message
}

/// Return the 32-byte raw public key, hex encoded for Nulang.trust.toml.
pub fn signing_public_key(secret_seed: &[u8; 32]) -> String {
    let signing_key = SigningKey::from_bytes(secret_seed);
    hex::encode(signing_key.verifying_key().as_bytes())
}

/// Publisher-side signature over registry URL + package coordinates + exact
/// compressed tarball digest. Only the secret seed signs; it is never sent.
pub fn sign_registry_archive(
    registry_url: &str,
    name: &str,
    version: &str,
    archive: &[u8],
    secret_seed: &[u8; 32],
) -> String {
    let signer = SigningKey::from_bytes(secret_seed);
    let message = signed_coordinates(registry_url, name, version, archive);
    hex::encode(signer.sign(&message).to_bytes())
}

/// Verify against at least one independently pinned publisher public key.
/// Valid signatures from other keys are insufficient.
pub fn verify_publisher_signature(
    registry_url: &str,
    name: &str,
    version: &str,
    archive: &[u8],
    hex_signature: &str,
    trusted_keys: &[String],
) -> Result<(), String> {
    if trusted_keys.is_empty() {
        return Err("registry has no configured trusted publisher keys".to_string());
    }
    let signature = hex::decode(hex_signature)
        .map_err(|_| "registry publisher signature is not hex".to_string())?;
    let signature = Signature::from_slice(&signature)
        .map_err(|_| "registry publisher signature must be 64 bytes".to_string())?;
    let message = signed_coordinates(registry_url, name, version, archive);

    // Reject malformed keys even if another key happens to verify.
    let keys = trusted_keys
        .iter()
        .map(|key| parse_verifying_key(key))
        .collect::<Result<Vec<_>, _>>()?;
    if keys.iter().any(|key| key.verify(&message, &signature).is_ok()) {
        Ok(())
    } else {
        Err(format!(
            "registry package '{}@{}' has no valid signature from a trusted publisher key",
            name, version
        ))
    }
}

/// Parse the seed from a protected environment variable. Do not echo seed
/// contents in diagnostics or write them into repository files.
pub fn signing_seed_from_env() -> Result<[u8; 32], String> {
    let encoded = std::env::var("NULA_SIGNING_KEY_HEX").map_err(|_| {
        "publishing requires NULA_SIGNING_KEY_HEX (32-byte Ed25519 signing seed)".to_string()
    })?;
    let bytes = hex::decode(encoded.trim())
        .map_err(|_| "NULA_SIGNING_KEY_HEX must contain exactly 64 hex characters".to_string())?;
    bytes.try_into().map_err(|_| {
        "NULA_SIGNING_KEY_HEX must contain exactly 32 bytes".to_string()
    })
}

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
