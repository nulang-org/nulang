//! Read-only offline authentication of cached registry packages.
//!
//! Signed original archives and detached signatures are stored adjacent to,
//! not inside, extracted packages. Offline mode validates signature, current
//! project trust policy, complete archive tree, extracted cache, and lockfile.
//! No network or filesystem writes occur during verification.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::package::identity::registry_tree_id_for_package_dir;
use crate::package::lockfile::{registry_cache_dir, Lockfile};
use crate::package::registry_archive::{
    registry_tree_id_from_signed_archive, MAX_REGISTRY_ARCHIVE_BYTES,
};
use crate::registry::trust::{
    trusted_publisher_keys, verify_publisher_signature,
};
use crate::types::{NuError, NuResult, Span};

static NEXT_RECEIPT: AtomicU64 = AtomicU64::new(0);

fn failure(msg: impl Into<String>) -> NuError {
    NuError::PackageError { msg: msg.into(), span: Span::default() }
}

/// Each publisher-authenticated compressed tarball is cached outside the
/// extracted source tree, preserving stable extracted SourceId values.
pub fn signed_provenance_dir(root: &Path, name: &str, version: &str) -> NuResult<PathBuf> {
    // Reuse the name/version validation boundary used by the registry cache.
    let path = registry_cache_dir(root, name, version)?;
    let name = path.file_name().ok_or_else(|| failure("invalid provenance path"))?;
    Ok(root.join(".nula").join("registry-provenance").join(name))
}

struct ReceiptStaging(PathBuf);
impl Drop for ReceiptStaging {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Store already publisher-authenticated package bytes and detached signature.
/// An existing receipt is immutable; a different archive for the same
/// package/version is a conflict rather than an implicit replacement.
pub fn store_signed_provenance(
    root: &Path,
    registry_url: &str,
    name: &str,
    version: &str,
    bytes: &[u8],
    signature: &str,
) -> NuResult<()> {
    if bytes.is_empty() || bytes.len() > MAX_REGISTRY_ARCHIVE_BYTES {
        return Err(failure("signed registry archive is empty or oversized"));
    }
    let keys = trusted_publisher_keys(root, registry_url).map_err(failure)?;
    verify_publisher_signature(registry_url, name, version, bytes, signature, &keys)
        .map_err(failure)?;
    // Reject invalid archives before persisting any receipt.
    registry_tree_id_from_signed_archive(bytes, name, version)?;

    let dest = signed_provenance_dir(root, name, version)?;
    if dest.exists() {
        let existing = fs::read(dest.join("archive.tar.gz"))
            .map_err(|e| failure(format!("cannot inspect signed registry receipt: {}", e)))?;
        let existing_sig = fs::read_to_string(dest.join("signature.hex"))
            .map_err(|e| failure(format!("cannot inspect signed registry signature: {}", e)))?;
        if existing == bytes && existing_sig == signature {
            return Ok(());
        }
        return Err(failure(format!(
            "signed registry receipt for '{}' {} conflicts with existing immutable receipt",
            name, version
        )));
    }

    let parent = dest.parent().ok_or_else(|| failure("signed receipt has no parent"))?;
    fs::create_dir_all(parent)
        .map_err(|e| failure(format!("cannot create signed receipt cache: {}", e)))?;
    let stage = loop {
        let n = NEXT_RECEIPT.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(format!(".receipt-{}-{}", std::process::id(), n));
        match fs::create_dir(&candidate) {
            Ok(()) => break ReceiptStaging(candidate),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(failure(format!("cannot stage signed receipt: {}", e))),
        }
    };
    fs::write(stage.0.join("archive.tar.gz"), bytes)
        .map_err(|e| failure(format!("cannot save signed archive: {}", e)))?;
    fs::write(stage.0.join("signature.hex"), signature)
        .map_err(|e| failure(format!("cannot save detached signature: {}", e)))?;
    fs::rename(&stage.0, &dest)
        .map_err(|e| failure(format!("cannot promote signed registry receipt: {}", e)))?;
    Ok(())
}

/// Verify every locked registry dependency as publisher-authentic and exactly
/// matching both Nulang.lock and the extracted cache. This function performs
/// only filesystem reads; it does not repair, fetch, unpack, or rewrite files.
pub fn verify_signed_provenance(root: &Path, lockfile: &Lockfile) -> NuResult<()> {
    for package in &lockfile.package {
        let Some(registry_url) = package.source.strip_prefix("reg+") else {
            continue;
        };
        let identity = lockfile
            .package_identity(&package.name, &package.version, &package.source)
            .ok_or_else(|| failure(format!(
                "offline signed provenance requires a strong Nulang.lock identity for '{}' {}",
                package.name, package.version
            )))?;
        let pinned = identity.parsed_source_id()?.ok_or_else(|| failure(format!(
            "offline signed provenance requires a nonempty SourceId for '{}' {}",
            package.name, package.version
        )))?;

        let receipt = signed_provenance_dir(root, &package.name, &package.version)?;
        let archive = fs::read(receipt.join("archive.tar.gz"))
            .map_err(|e| failure(format!(
                "offline signed archive missing for '{}' {}: {}",
                package.name, package.version, e
            )))?;
        if archive.is_empty() || archive.len() > MAX_REGISTRY_ARCHIVE_BYTES {
            return Err(failure("offline signed archive is missing or oversized"));
        }
        let signature = fs::read_to_string(receipt.join("signature.hex"))
            .map_err(|e| failure(format!(
                "offline publisher signature missing for '{}' {}: {}",
                package.name, package.version, e
            )))?;
        let keys = trusted_publisher_keys(root, registry_url).map_err(failure)?;
        verify_publisher_signature(
            registry_url, &package.name, &package.version,
            &archive, &signature, &keys
        ).map_err(failure)?;

        // Explicitly bind the authenticated tarball to the locked extracted
        // tree, not merely to a signature on some unrelated valid archive.
        let signed_tree = registry_tree_id_from_signed_archive(
            &archive, &package.name, &package.version
        )?;
        let extracted = registry_tree_id_for_package_dir(
            &registry_cache_dir(root, &package.name, &package.version)?
        ).map_err(|e| failure(format!(
            "cannot verify offline registry cache for '{}' {}: {}",
            package.name, package.version, e
        )))?;
        if signed_tree != pinned || extracted != pinned {
            return Err(failure(format!(
                "offline registry package '{}' {} does not match authenticated archive and Nulang.lock",
                package.name, package.version
            )));
        }
    }
    Ok(())
}

// Signed cache provenance regression tests.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::package::manifest::MANIFEST_FILE;
    use crate::package::registry_archive::extract_registry_archive;
    use crate::package::lockfile::{Lockfile, LockedPackage};

    fn fixture(tag: &str) -> (std::path::PathBuf, Vec<u8>, String, Lockfile) {
        let root = std::env::temp_dir().join(format!(
            "nula_signed_provenance_{}_{}", tag, std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        let gzip = flate2::write::GzEncoder::new(
            Vec::new(), flate2::Compression::default()
        );
        let mut tar = tar::Builder::new(gzip);
        for (path, data) in [
            (MANIFEST_FILE, "[package]\nname = \"util\"\nversion = \"0.1.0\"\n"),
            ("src/lib.nula", "fn main() { 1 }"),
            ("assets/meta.txt", "signed static assets"),
        ] {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            tar.append_data(&mut header, path, data.as_bytes()).unwrap();
        }
        let archive = tar.into_inner().unwrap().finish().unwrap();
        let url = "https://registry.test";
        let key = [23u8; 32];
        let signature = crate::registry::trust::sign_registry_archive(
            url, "util", "0.1.0", &archive, &key
        );
        let public = crate::registry::trust::signing_public_key(&key);
        std::fs::write(
            root.join("Nulang.trust.toml"),
            format!("[registries.\"{}\"]\npublic_keys = [\"{}\"]\n", url, public),
        ).unwrap();
        let cache = crate::package::lockfile::registry_cache_dir(
            &root, "util", "0.1.0"
        ).unwrap();
        extract_registry_archive(&archive, &cache, "util", "0.1.0").unwrap();

        let mut lock = Lockfile::new();
        lock.package.push(LockedPackage {
            name: "util".into(),
            version: "0.1.0".into(),
            source: format!("reg+{}", url),
            content_hash: String::new(),
            commit: String::new(),
        });
        lock.save(&root).unwrap();
        (root.clone(), archive, signature, Lockfile::load(&root).unwrap())
    }

    #[test]
    fn signed_provenance_offline_accepts_only_authenticated_matching_artifact() {
        let (root, archive, signature, lock) = fixture("valid");
        store_signed_provenance(
            &root, "https://registry.test", "util", "0.1.0", &archive, &signature
        ).unwrap();
        verify_signed_provenance(&root, &lock).unwrap();
        let before = std::fs::read(root.join("Nulang.lock")).unwrap();
        verify_signed_provenance(&root, &lock).unwrap();
        assert_eq!(before, std::fs::read(root.join("Nulang.lock")).unwrap());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn signed_provenance_offline_rejects_missing_receipt() {
        let (root, _archive, _signature, lock) = fixture("missing");
        assert!(verify_signed_provenance(&root, &lock).is_err());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn signed_provenance_offline_rejects_revoked_publisher_key() {
        let (root, archive, signature, lock) = fixture("revoked");
        store_signed_provenance(
            &root, "https://registry.test", "util", "0.1.0", &archive, &signature
        ).unwrap();
        std::fs::write(
            root.join("Nulang.trust.toml"),
            "[registries.\"https://registry.test\"]\npublic_keys = [\"00".to_string()
                + &"00".repeat(31) + "\"]\n"
        ).unwrap();
        assert!(verify_signed_provenance(&root, &lock).is_err());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn signed_provenance_offline_rejects_archive_signature_or_cached_tree_drift() {
        let (root, archive, signature, lock) = fixture("tamper");
        store_signed_provenance(
            &root, "https://registry.test", "util", "0.1.0", &archive, &signature
        ).unwrap();
        let receipt = signed_provenance_dir(&root, "util", "0.1.0").unwrap();
        let tar_file = receipt.join("archive.tar.gz");
        let mut bad_archive = archive.clone();
        bad_archive[5] ^= 0x01;
        std::fs::write(&tar_file, &bad_archive).unwrap();
        assert!(verify_signed_provenance(&root, &lock).is_err());
        std::fs::write(&tar_file, &archive).unwrap();
        std::fs::write(receipt.join("signature.hex"), "ab".repeat(64)).unwrap();
        assert!(verify_signed_provenance(&root, &lock).is_err());
        std::fs::write(receipt.join("signature.hex"), &signature).unwrap();
        std::fs::write(
            root.join(".nula/registry/util-0.1.0/assets/meta.txt"), "tampered"
        ).unwrap();
        assert!(verify_signed_provenance(&root, &lock).is_err());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn signed_provenance_offline_rejects_wrong_signed_archive_for_cached_source() {
        let (root, archive, signature, mut lock) = fixture("unrelated");
        store_signed_provenance(
            &root, "https://registry.test", "util", "0.1.0", &archive, &signature
        ).unwrap();
        let id = crate::content_identity::SourceId::from_bytes(b"unrelated tree");
        lock.set_package_identity(
            "util", "0.1.0", "reg+https://registry.test", Some(id), None
        ).unwrap();
        assert!(verify_signed_provenance(&root, &lock).is_err());
        let _ = std::fs::remove_dir_all(root);
    }
}
