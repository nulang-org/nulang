//! Safe extraction of untrusted Nula registry archives.
//! The archive is staged separately and only promoted after full validation.

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn fresh_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "nula_registry_archive_{}_{}", tag, std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        root
    }

    fn archive_with_files(files: &[(&str, &str)]) -> Vec<u8> {
        let gzip = flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::default(),
        );
        let mut tar = tar::Builder::new(gzip);
        for &(name, content) in files {
            let mut header = tar::Header::new_gnu();
            header.set_mode(0o644);
            header.set_size(content.len() as u64);
            header.set_cksum();
            tar.append_data(&mut header, name, content.as_bytes()).unwrap();
        }
        tar.into_inner().unwrap().finish().unwrap()
    }

    fn valid_archive() -> Vec<u8> {
        archive_with_files(&[
            ("Nulang.toml", "[package]\nname = \"util\"\nversion = \"0.1.0\"\n"),
            ("src/lib.nula", "fn main() { 1 }"),
            ("assets/data.json", "{\"key\":true}"),
        ])
    }

    #[test]
    fn safe_registry_archive_extracts_valid_sources_and_static_assets() {
        let root = fresh_root("valid");
        let dest = root.join(".nula/registry/util-0.1.0");
        extract_registry_archive(&valid_archive(), &dest, "util", "0.1.0").unwrap();
        assert!(dest.join("Nulang.toml").is_file());
        assert_eq!(
            std::fs::read_to_string(dest.join("assets/data.json")).unwrap(),
            "{\"key\":true}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn safe_registry_archive_rejects_symlink_and_leaves_no_partial_cache() {
        let root = fresh_root("symlink");
        let dest = root.join(".nula/registry/util-0.1.0");
        let gzip = flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::default(),
        );
        let mut tar = tar::Builder::new(gzip);
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_size(0);
        header.set_mode(0o777);
        header.set_path("src/escape").unwrap();
        header.set_link_name("../../outside").unwrap();
        header.set_cksum();
        tar.append(&header, std::io::empty()).unwrap();
        let bytes = tar.into_inner().unwrap().finish().unwrap();

        assert!(extract_registry_archive(&bytes, &dest, "util", "0.1.0").is_err());
        assert!(!dest.exists());
        assert!(!root.join("outside").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn safe_registry_archive_rejects_traversal_paths() {
        let root = fresh_root("traversal");
        let dest = root.join(".nula/registry/util-0.1.0");
        let gzip = flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::default(),
        );
        let mut tar = tar::Builder::new(gzip);
        let mut header = tar::Header::new_gnu();
        header.set_size(0);
        header.set_mode(0o644);
        let name = b"../outside";
        header.as_mut_bytes()[..name.len()].copy_from_slice(name);
        header.set_cksum();
        tar.append(&header, std::io::empty()).unwrap();
        let bytes = tar.into_inner().unwrap().finish().unwrap();

        assert!(extract_registry_archive(&bytes, &dest, "util", "0.1.0").is_err());
        assert!(!dest.exists());
        assert!(!root.join("outside").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn safe_registry_archive_rejects_incorrect_manifest_without_overwriting_cache() {
        let root = fresh_root("mismatch");
        let dest = root.join(".nula/registry/util-0.1.0");
        let bytes = archive_with_files(&[
            ("Nulang.toml", "[package]\nname = \"evil\"\nversion = \"0.1.0\"\n"),
        ]);
        assert!(extract_registry_archive(&bytes, &dest, "util", "0.1.0").is_err());
        assert!(!dest.exists());

        extract_registry_archive(&valid_archive(), &dest, "util", "0.1.0").unwrap();
        assert!(extract_registry_archive(&bytes, &dest, "util", "0.1.0").is_err());
        assert_eq!(
            std::fs::read_to_string(dest.join("src/lib.nula")).unwrap(),
            "fn main() { 1 }"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn safe_registry_archive_rejects_truncated_gzip_without_cache() {
        let root = fresh_root("truncated");
        let dest = root.join(".nula/registry/util-0.1.0");
        let mut bytes = valid_archive();
        bytes.truncate(bytes.len() - 6);
        assert!(extract_registry_archive(&bytes, &dest, "util", "0.1.0").is_err());
        assert!(!dest.exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn safe_registry_archive_rejects_oversized_entry_before_writing_it() {
        let root = fresh_root("size");
        let dest = root.join(".nula/registry/util-0.1.0");
        let gzip = flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::default(),
        );
        let mut tar = tar::Builder::new(gzip);
        let mut header = tar::Header::new_gnu();
        header.set_path("huge.nula").unwrap();
        header.set_mode(0o644);
        header.set_size(MAX_ENTRY_BYTES + 1);
        header.set_cksum();
        // An intentionally malformed archive advertises an excessive size.
        let mut raw = header.as_bytes().to_vec();
        raw.extend_from_slice(&[0u8; 1024]);
        let mut gz = flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::default(),
        );
        gz.write_all(&raw).unwrap();
        let bytes = gz.finish().unwrap();
        let _ = tar; // Header construction uses tar's supported format.
        assert!(extract_registry_archive(&bytes, &dest, "util", "0.1.0").is_err());
        assert!(!dest.exists());
        let _ = std::fs::remove_dir_all(&root);
    }
}
