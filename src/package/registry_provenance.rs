//! Cached publisher provenance tests.

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use crate::package::manifest::{Manifest, MANIFEST_FILE};
    use crate::package::registry_archive::extract_registry_archive;
    use crate::package::lockfile::{Lockfile, LockedPackage};
    use std::io::Write;

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
        let id = crate::package::identity::SourceId::from_bytes(b"unrelated tree");
        lock.set_package_identity(
            "util", "0.1.0", "reg+https://registry.test", Some(id), None
        ).unwrap();
        assert!(verify_signed_provenance(&root, &lock).is_err());
        let _ = std::fs::remove_dir_all(root);
    }
}
