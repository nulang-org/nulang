//! Bounded, no-links, atomic extraction for untrusted registry archives.
//! Archives are fully validated in a fresh staging directory before becoming
//! addressable by the package resolver.

use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::package::manifest::Manifest;
use crate::types::{NuError, NuResult, Span};

/// Limits apply both to compressed network responses and extracted bytes.
pub const MAX_REGISTRY_ARCHIVE_BYTES: usize = 64 * 1024 * 1024;
const MAX_EXTRACTED_BYTES: u64 = 128 * 1024 * 1024;
const MAX_DECOMPRESSED_STREAM_BYTES: u64 = 160 * 1024 * 1024;
const MAX_ENTRIES: usize = 2048;
const MAX_ENTRY_BYTES: u64 = 32 * 1024 * 1024;

static NEXT_STAGING_ID: AtomicU64 = AtomicU64::new(0);

fn package_error(message: impl Into<String>) -> NuError {
    NuError::PackageError {
        msg: message.into(),
        span: Span::default(),
    }
}

struct StagingDirectory(PathBuf);

impl Drop for StagingDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Extract a downloaded tar.gz package to a unique sibling staging directory.
/// On any failure (bad gzip, traversal, links, invalid manifest, or limits)
/// only that staging directory is removed. An existing cache is never erased.
pub fn extract_registry_archive(
    tarball: &[u8],
    dest: &Path,
    expected_name: &str,
    expected_version: &str,
) -> NuResult<PathBuf> {
    if tarball.is_empty() || tarball.len() > MAX_REGISTRY_ARCHIVE_BYTES {
        return Err(package_error(format!(
            "registry archive length {} outside allowed range (1..={} bytes)",
            tarball.len(),
            MAX_REGISTRY_ARCHIVE_BYTES
        )));
    }
    if dest.exists() {
        return Err(package_error(format!(
            "registry cache destination already exists: {}",
            dest.display()
        )));
    }

    let parent = dest.parent().ok_or_else(|| {
        package_error(format!("registry cache destination has no parent: {}", dest.display()))
    })?;
    fs::create_dir_all(parent).map_err(|e| {
        package_error(format!("cannot create registry cache {}: {}", parent.display(), e))
    })?;

    let stage = loop {
        let sequence = NEXT_STAGING_ID.fetch_add(1, Ordering::Relaxed);
        let name = dest.file_name().and_then(|name| name.to_str()).ok_or_else(|| {
            package_error(format!("invalid registry cache destination: {}", dest.display()))
        })?;
        let candidate = parent.join(format!(
            ".{}.staging-{}-{}", name, std::process::id(), sequence
        ));
        match fs::create_dir(&candidate) {
            Ok(()) => break StagingDirectory(candidate),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(package_error(format!(
                "cannot create registry staging directory {}: {}",
                candidate.display(), error
            ))),
        }
    };

    let decompress = flate2::read::GzDecoder::new(tarball);
    let limited = decompress.take(MAX_DECOMPRESSED_STREAM_BYTES + 1);
    let mut archive = tar::Archive::new(limited);
    let mut entries = 0usize;
    let mut extracted_bytes = 0u64;
    let mut seen = BTreeSet::<PathBuf>::new();

    for entry in archive.entries().map_err(|e| package_error(format!(
        "invalid registry tar archive: {}", e
    )))? {
        let mut entry = entry.map_err(|e| package_error(format!(
            "invalid registry tar entry: {}", e
        )))?;
        entries += 1;
        if entries > MAX_ENTRIES {
            return Err(package_error(format!(
                "registry archive has too many entries (limit {})", MAX_ENTRIES
            )));
        }

        let relative = entry.path().map_err(|e| package_error(format!(
            "invalid registry entry path: {}", e
        )))?.into_owned();
        let valid = !relative.as_os_str().is_empty()
            && relative.components().all(|component| match component {
                Component::Normal(part) => {
                    !part.to_string_lossy().contains('\\')
                }
                _ => false,
            });
        if !valid || !seen.insert(relative.clone()) {
            return Err(package_error(format!(
                "registry archive contains unsafe or repeated path: {}",
                relative.display()
            )));
        }

        let target = stage.0.join(&relative);
        let kind = entry.header().entry_type();
        if kind.is_dir() {
            fs::create_dir_all(&target).map_err(|e| package_error(format!(
                "cannot create registry directory {}: {}", target.display(), e
            )))?;
        } else if kind.is_file() {
            let size = entry.size();
            extracted_bytes = extracted_bytes.checked_add(size).ok_or_else(|| {
                package_error("registry archive extracted size overflows")
            })?;
            if size > MAX_ENTRY_BYTES || extracted_bytes > MAX_EXTRACTED_BYTES {
                return Err(package_error(format!(
                    "registry archive exceeds file or total extracted-size limit: {}",
                    relative.display()
                )));
            }
            if let Some(dir) = target.parent() {
                fs::create_dir_all(dir).map_err(|e| package_error(format!(
                    "cannot create registry directory {}: {}", dir.display(), e
                )))?;
            }
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&target)
                .map_err(|e| package_error(format!(
                    "cannot create registry file {}: {}", target.display(), e
                )))?;
            let copied = io::copy(&mut entry, &mut file).map_err(|e| package_error(format!(
                "cannot extract registry file {}: {}", target.display(), e
            )))?;
            if copied != size {
                return Err(package_error(format!(
                    "truncated registry file {}: expected {} bytes, received {}",
                    relative.display(), size, copied
                )));
            }
        } else {
            // Never materialize symlinks, hard links, devices, or FIFOs.
            return Err(package_error(format!(
                "registry archive contains unsupported entry type at {}",
                relative.display()
            )));
        }
    }

    // Force decoding to the end, including gzip CRC/trailer validation.
    let mut stream = archive.into_inner();
    io::copy(&mut stream, &mut io::sink()).map_err(|e| package_error(format!(
        "invalid or truncated registry gzip stream: {}", e
    )))?;
    if stream.limit() == 0 {
        return Err(package_error("registry archive exceeds decompression stream limit"));
    }

    let manifest = Manifest::load(&stage.0).map_err(|e| package_error(format!(
        "registry archive missing or invalid Nulang.toml: {}", e
    )))?;
    if manifest.package.name != expected_name || manifest.package.version != expected_version {
        return Err(package_error(format!(
            "registry archive metadata does not match requested package {} {}",
            expected_name, expected_version
        )));
    }
    if dest.exists() {
        return Err(package_error(format!(
            "registry cache destination appeared during extraction: {}",
            dest.display()
        )));
    }

    fs::rename(&stage.0, dest).map_err(|e| package_error(format!(
        "cannot promote verified registry cache {}: {}", dest.display(), e
    )))?;
    fs::canonicalize(dest).map_err(|e| package_error(format!(
        "cannot canonicalize verified registry cache {}: {}", dest.display(), e
    )))
}

/// Recompute the canonical extracted tree identity from a signed tar.gz
/// entirely in memory. No files or directories are created in offline mode.
/// Bounds and entry restrictions mirror `extract_registry_archive`.
pub fn registry_tree_id_from_signed_archive(
    tarball: &[u8],
    expected_name: &str,
    expected_version: &str,
) -> NuResult<crate::content_identity::SourceId> {
    if tarball.is_empty() || tarball.len() > MAX_REGISTRY_ARCHIVE_BYTES {
        return Err(package_error("signed registry archive is missing or too large"));
    }

    // Finish the GZIP stream first, validating CRC before accepting entries.
    let mut gzip = flate2::read::GzDecoder::new(tarball);
    let mut raw = Vec::new();
    gzip.by_ref().take(MAX_DECOMPRESSED_STREAM_BYTES + 1)
        .read_to_end(&mut raw)
        .map_err(|e| package_error(format!("invalid signed registry gzip: {}", e)))?;
    if raw.len() as u64 > MAX_DECOMPRESSED_STREAM_BYTES {
        return Err(package_error("signed registry archive exceeds decompression limit"));
    }

    let mut archive = tar::Archive::new(std::io::Cursor::new(raw));
    let mut seen = BTreeSet::new();
    let mut files = Vec::new();
    let mut total = 0u64;
    let mut entries = 0usize;
    let mut manifest_data = None;

    for entry in archive.entries().map_err(|e| package_error(format!(
        "invalid signed registry tar: {}", e
    )))? {
        let mut entry = entry.map_err(|e| package_error(format!(
            "invalid signed registry entry: {}", e
        )))?;
        entries += 1;
        if entries > MAX_ENTRIES {
            return Err(package_error("signed registry archive has too many entries"));
        }
        let relative = entry.path().map_err(|e| package_error(format!(
            "invalid signed registry path: {}", e
        )))?.into_owned();
        let valid = !relative.as_os_str().is_empty()
            && relative.components().all(|component| match component {
                Component::Normal(part) => !part.to_string_lossy().contains('\\'),
                _ => false,
            });
        if !valid || !seen.insert(relative.clone()) {
            return Err(package_error(format!(
                "signed registry archive has unsafe or duplicate path: {}",
                relative.display()
            )));
        }
        if entry.header().entry_type().is_dir() {
            continue;
        }
        if !entry.header().entry_type().is_file() {
            return Err(package_error(format!(
                "signed registry archive contains link or special entry: {}",
                relative.display()
            )));
        }
        let size = entry.size();
        total = total.checked_add(size)
            .ok_or_else(|| package_error("signed registry archive extracted size overflow"))?;
        if size > MAX_ENTRY_BYTES || total > MAX_EXTRACTED_BYTES {
            return Err(package_error("signed registry archive exceeds extracted file limits"));
        }
        let name = relative.to_str()
            .ok_or_else(|| package_error("signed registry path is not valid UTF-8"))?
            .replace('\\', "/");
        let mut contents = Vec::with_capacity(size as usize);
        entry.read_to_end(&mut contents).map_err(|e| package_error(format!(
            "cannot read signed registry entry '{}': {}", name, e
        )))?;
        if contents.len() as u64 != size {
            return Err(package_error(format!("truncated signed registry entry '{}'", name)));
        }
        if name == "Nulang.toml" {
            manifest_data = Some(contents.clone());
        }
        files.push((name, contents));
    }
    let manifest_bytes = manifest_data
        .ok_or_else(|| package_error("signed registry archive has no Nulang.toml"))?;
    let manifest_text = std::str::from_utf8(&manifest_bytes)
        .map_err(|_| package_error("signed registry manifest is not UTF-8"))?;
    let manifest = Manifest::parse(manifest_text)?;
    if manifest.package.name != expected_name || manifest.package.version != expected_version {
        return Err(package_error("signed registry archive package metadata differs from lockfile"));
    }
    Ok(crate::package::identity::registry_tree_id_from_entries(&files))
}

// Regression tests for archive validation and staging behavior.

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
    fn safe_registry_archive_rejects_duplicate_paths() {
        let root = fresh_root("duplicate");
        let dest = root.join(".nula/registry/util-0.1.0");
        let bytes = archive_with_files(&[
            ("Nulang.toml", "[package]\nname = \"util\"\nversion = \"0.1.0\"\n"),
            ("Nulang.toml", "[package]\nname = \"evil\"\nversion = \"0.1.0\"\n"),
        ]);
        assert!(extract_registry_archive(&bytes, &dest, "util", "0.1.0").is_err());
        assert!(!dest.exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn safe_registry_archive_limits_entry_count() {
        let root = fresh_root("entry_count");
        let dest = root.join(".nula/registry/util-0.1.0");
        let gzip = flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::default(),
        );
        let mut tar = tar::Builder::new(gzip);
        for index in 0..=MAX_ENTRIES {
            let mut header = tar::Header::new_gnu();
            header.set_mode(0o644);
            header.set_size(0);
            header.set_cksum();
            tar.append_data(
                &mut header,
                format!("src/file{}.nula", index),
                std::io::empty(),
            ).unwrap();
        }
        let bytes = tar.into_inner().unwrap().finish().unwrap();
        assert!(extract_registry_archive(&bytes, &dest, "util", "0.1.0").is_err());
        assert!(!dest.exists());
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
        assert!(extract_registry_archive(&bytes, &dest, "util", "0.1.0").is_err());
        assert!(!dest.exists());
        let _ = std::fs::remove_dir_all(&root);
    }
}
