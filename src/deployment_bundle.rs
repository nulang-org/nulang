//! Hardened parser for the gzip/tar deployment bundle produced by `nula deploy`.
//!
//! The parser never extracts archive contents to disk. It validates archive
//! paths and entry types, enforces compressed/expanded/resource limits, binds
//! `Nulang.toml` package identity to exactly one packaged `.nbc`, derives the
//! compiled execution manifest, and can feed the artifact directly into the
//! Cloud admission engine.

use std::fmt;
use std::io::{Cursor, Read};
use std::path::{Component, Path};

use flate2::read::GzDecoder;

use crate::admission_policy::{
    evaluate_artifact_admission, AdmissionDecision, AdmissionEnvironment, AdmissionPolicy,
};
use crate::deployment_manifest::CompiledExecutionManifest;
use crate::package::manifest::Manifest;
use crate::web::ir::{DeploymentIr, INCOMPLETE_METADATA_CAPABILITY};

pub const MAX_DEPLOYMENT_BUNDLE_BYTES: usize = 256 * 1024 * 1024;
pub const MAX_DEPLOYMENT_EXPANDED_BYTES: u64 = 2 * 1024 * 1024 * 1024;
pub const MAX_DEPLOYMENT_ENTRIES: usize = 100_000;
pub const MAX_NBC_ARTIFACT_BYTES: u64 = 256 * 1024 * 1024;
pub const MAX_PACKAGE_MANIFEST_BYTES: u64 = 1024 * 1024;
pub const MAX_DEPLOYMENT_IR_BYTES: u64 = 8 * 1024 * 1024;

/// Validated deployment inputs extracted from a bundle without writing to disk.
#[derive(Debug)]
pub struct DeploymentBundle {
    pub package_manifest: Manifest,
    pub artifact_path: String,
    pub artifact_bytes: Vec<u8>,
    pub execution_manifest: CompiledExecutionManifest,
    pub web_ir: Option<DeploymentIr>,
    pub entry_count: usize,
    pub expanded_bytes: u64,
}

impl DeploymentBundle {
    /// Parse and validate the current `nula deploy` gzip/tar wire format.
    pub fn parse(bytes: &[u8]) -> Result<Self, DeploymentBundleError> {
        if bytes.len() > MAX_DEPLOYMENT_BUNDLE_BYTES {
            return Err(DeploymentBundleError::CompressedTooLarge {
                actual: bytes.len(),
                max: MAX_DEPLOYMENT_BUNDLE_BYTES,
            });
        }

        let decoder = GzDecoder::new(Cursor::new(bytes));
        let mut archive = tar::Archive::new(decoder);
        let entries = archive
            .entries()
            .map_err(|err| DeploymentBundleError::Archive(err.to_string()))?;

        let mut entry_count = 0usize;
        let mut expanded_bytes = 0u64;
        let mut package_manifest: Option<Manifest> = None;
        let mut artifact: Option<(String, Vec<u8>)> = None;
        let mut web_ir: Option<DeploymentIr> = None;

        for next in entries {
            entry_count = entry_count
                .checked_add(1)
                .ok_or(DeploymentBundleError::TooManyEntries {
                    actual: usize::MAX,
                    max: MAX_DEPLOYMENT_ENTRIES,
                })?;
            if entry_count > MAX_DEPLOYMENT_ENTRIES {
                return Err(DeploymentBundleError::TooManyEntries {
                    actual: entry_count,
                    max: MAX_DEPLOYMENT_ENTRIES,
                });
            }

            let mut entry =
                next.map_err(|err| DeploymentBundleError::Archive(err.to_string()))?;
            let entry_type = entry.header().entry_type();
            if !entry_type.is_file() && !entry_type.is_dir() {
                let path = entry
                    .path()
                    .map(|path| path.to_string_lossy().into_owned())
                    .unwrap_or_else(|_| "<invalid-path>".to_string());
                return Err(DeploymentBundleError::UnsupportedEntryType { path });
            }

            let path = entry
                .path()
                .map_err(|err| DeploymentBundleError::Archive(err.to_string()))?
                .into_owned();
            validate_archive_path(&path)?;
            let path_string = path.to_string_lossy().into_owned();

            let size = entry.size();
            expanded_bytes = expanded_bytes
                .checked_add(size)
                .ok_or(DeploymentBundleError::ExpandedTooLarge {
                    actual: u64::MAX,
                    max: MAX_DEPLOYMENT_EXPANDED_BYTES,
                })?;
            if expanded_bytes > MAX_DEPLOYMENT_EXPANDED_BYTES {
                return Err(DeploymentBundleError::ExpandedTooLarge {
                    actual: expanded_bytes,
                    max: MAX_DEPLOYMENT_EXPANDED_BYTES,
                });
            }

            if !entry_type.is_file() {
                continue;
            }

            if path == Path::new("Nulang.toml") {
                if package_manifest.is_some() {
                    return Err(DeploymentBundleError::MultiplePackageManifests);
                }
                if size > MAX_PACKAGE_MANIFEST_BYTES {
                    return Err(DeploymentBundleError::PackageManifestTooLarge {
                        actual: size,
                        max: MAX_PACKAGE_MANIFEST_BYTES,
                    });
                }
                let data = read_limited(&mut entry, MAX_PACKAGE_MANIFEST_BYTES).map_err(|err| {
                    DeploymentBundleError::Archive(format!(
                        "cannot read {path_string}: {err}"
                    ))
                })?;
                let source = std::str::from_utf8(&data).map_err(|err| {
                    DeploymentBundleError::InvalidPackageManifest(err.to_string())
                })?;
                let parsed = Manifest::parse(source).map_err(|err| {
                    DeploymentBundleError::InvalidPackageManifest(err.to_string())
                })?;
                package_manifest = Some(parsed);
                continue;
            }

            if is_packaged_nbc(&path) {
                if artifact.is_some() {
                    return Err(DeploymentBundleError::MultipleArtifacts);
                }
                if size > MAX_NBC_ARTIFACT_BYTES {
                    return Err(DeploymentBundleError::ArtifactTooLarge {
                        actual: size,
                        max: MAX_NBC_ARTIFACT_BYTES,
                    });
                }
                let data = read_limited(&mut entry, MAX_NBC_ARTIFACT_BYTES).map_err(|err| {
                    DeploymentBundleError::Archive(format!(
                        "cannot read {path_string}: {err}"
                    ))
                })?;
                artifact = Some((path_string, data));
                continue;
            }

            if path == Path::new("dist/nulang-app.ir.json") {
                if web_ir.is_some() {
                    return Err(DeploymentBundleError::MultipleDeploymentIr);
                }
                if size > MAX_DEPLOYMENT_IR_BYTES {
                    return Err(DeploymentBundleError::DeploymentIrTooLarge {
                        actual: size,
                        max: MAX_DEPLOYMENT_IR_BYTES,
                    });
                }
                let data = read_limited(&mut entry, MAX_DEPLOYMENT_IR_BYTES).map_err(|err| {
                    DeploymentBundleError::Archive(format!(
                        "cannot read {path_string}: {err}"
                    ))
                })?;
                let parsed: DeploymentIr = serde_json::from_slice(&data)
                    .map_err(|err| DeploymentBundleError::InvalidDeploymentIr(err.to_string()))?;
                if parsed
                    .capabilities
                    .iter()
                    .any(|cap| cap == INCOMPLETE_METADATA_CAPABILITY)
                {
                    return Err(DeploymentBundleError::IncompleteDeploymentMetadata);
                }
                web_ir = Some(parsed);
            }
        }

        let package_manifest =
            package_manifest.ok_or(DeploymentBundleError::MissingPackageManifest)?;
        let (artifact_path, artifact_bytes) =
            artifact.ok_or(DeploymentBundleError::MissingArtifact)?;
        let expected_artifact = Path::new(".nula/dist")
            .join(format!("{}.nbc", package_manifest.package.name))
            .to_string_lossy()
            .into_owned();
        if artifact_path != expected_artifact {
            return Err(DeploymentBundleError::ArtifactNameMismatch {
                expected: expected_artifact,
                actual: artifact_path,
            });
        }

        let execution_manifest = CompiledExecutionManifest::from_nbc_bytes(&artifact_bytes)
            .map_err(DeploymentBundleError::InvalidArtifact)?;

        Ok(Self {
            package_manifest,
            artifact_path,
            artifact_bytes,
            execution_manifest,
            web_ir,
            entry_count,
            expanded_bytes,
        })
    }

    /// Run fail-closed Cloud admission against the exact artifact bytes from
    /// this validated bundle. The manifest is re-derived inside admission.
    pub fn evaluate_admission(
        &self,
        policy: &AdmissionPolicy,
        environment: &AdmissionEnvironment,
    ) -> AdmissionDecision {
        evaluate_artifact_admission(&self.artifact_bytes, policy, environment)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum DeploymentBundleError {
    CompressedTooLarge { actual: usize, max: usize },
    ExpandedTooLarge { actual: u64, max: u64 },
    TooManyEntries { actual: usize, max: usize },
    UnsafePath { path: String },
    UnsupportedEntryType { path: String },
    MissingPackageManifest,
    MultiplePackageManifests,
    PackageManifestTooLarge { actual: u64, max: u64 },
    InvalidPackageManifest(String),
    MissingArtifact,
    MultipleArtifacts,
    ArtifactNameMismatch { expected: String, actual: String },
    ArtifactTooLarge { actual: u64, max: u64 },
    InvalidArtifact(String),
    MultipleDeploymentIr,
    DeploymentIrTooLarge { actual: u64, max: u64 },
    InvalidDeploymentIr(String),
    IncompleteDeploymentMetadata,
    Archive(String),
}

impl fmt::Display for DeploymentBundleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CompressedTooLarge { actual, max } => {
                write!(f, "deployment bundle is {actual} bytes; maximum is {max}")
            }
            Self::ExpandedTooLarge { actual, max } => {
                write!(f, "expanded deployment is {actual} bytes; maximum is {max}")
            }
            Self::TooManyEntries { actual, max } => {
                write!(f, "deployment bundle has {actual} entries; maximum is {max}")
            }
            Self::UnsafePath { path } => write!(f, "unsafe archive path: {path}"),
            Self::UnsupportedEntryType { path } => {
                write!(f, "unsupported archive entry type at {path}")
            }
            Self::MissingPackageManifest => write!(f, "deployment bundle is missing Nulang.toml"),
            Self::MultiplePackageManifests => {
                write!(f, "deployment bundle contains multiple Nulang.toml entries")
            }
            Self::PackageManifestTooLarge { actual, max } => {
                write!(f, "Nulang.toml is {actual} bytes; maximum is {max}")
            }
            Self::InvalidPackageManifest(message) => {
                write!(f, "invalid Nulang.toml: {message}")
            }
            Self::MissingArtifact => write!(f, "deployment bundle contains no .nula/dist/*.nbc"),
            Self::MultipleArtifacts => {
                write!(f, "deployment bundle contains multiple .nula/dist/*.nbc artifacts")
            }
            Self::ArtifactNameMismatch { expected, actual } => write!(
                f,
                "compiled artifact path {actual} does not match package manifest; expected {expected}"
            ),
            Self::ArtifactTooLarge { actual, max } => {
                write!(f, ".nbc artifact is {actual} bytes; maximum is {max}")
            }
            Self::InvalidArtifact(message) => write!(f, "invalid .nbc artifact: {message}"),
            Self::MultipleDeploymentIr => {
                write!(f, "deployment bundle contains duplicate nulang-app.ir.json")
            }
            Self::DeploymentIrTooLarge { actual, max } => write!(
                f,
                "deployment IR is {actual} bytes; maximum is {max}"
            ),
            Self::InvalidDeploymentIr(message) => write!(f, "invalid deployment IR: {message}"),
            Self::IncompleteDeploymentMetadata => write!(
                f,
                "deployment IR contains incomplete semantic metadata and cannot be admitted"
            ),
            Self::Archive(message) => write!(f, "invalid deployment archive: {message}"),
        }
    }
}

impl std::error::Error for DeploymentBundleError {}

fn validate_archive_path(path: &Path) -> Result<(), DeploymentBundleError> {
    if path.as_os_str().is_empty() || path.is_absolute() {
        return Err(DeploymentBundleError::UnsafePath {
            path: path.to_string_lossy().into_owned(),
        });
    }
    for component in path.components() {
        if !matches!(component, Component::Normal(_)) {
            return Err(DeploymentBundleError::UnsafePath {
                path: path.to_string_lossy().into_owned(),
            });
        }
    }
    Ok(())
}

fn is_packaged_nbc(path: &Path) -> bool {
    path.starts_with(Path::new(".nula/dist"))
        && path.extension().and_then(|ext| ext.to_str()) == Some("nbc")
}

fn read_limited<R: Read>(reader: &mut R, max: u64) -> std::io::Result<Vec<u8>> {
    let mut data = Vec::new();
    let mut limited = reader.take(max.saturating_add(1));
    limited.read_to_end(&mut data)?;
    if data.len() as u64 > max {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "entry exceeds configured size limit",
        ));
    }
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bytecode::CodeModule;
    use flate2::write::GzEncoder;
    use flate2::Compression;

    fn package_manifest(name: &str) -> Vec<u8> {
        format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\n").into_bytes()
    }

    fn make_bundle(mut entries: Vec<(&str, Vec<u8>)>) -> Vec<u8> {
        if !entries.iter().any(|(path, _)| *path == "Nulang.toml") {
            entries.insert(0, ("Nulang.toml", package_manifest("app")));
        }
        let mut output = Vec::new();
        {
            let gzip = GzEncoder::new(&mut output, Compression::default());
            let mut builder = tar::Builder::new(gzip);
            for (path, data) in entries {
                let mut header = tar::Header::new_gnu();
                header.set_size(data.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                builder
                    .append_data(&mut header, path, data.as_slice())
                    .expect("append entry");
            }
            let gzip = builder.into_inner().expect("finish tar");
            gzip.finish().expect("finish gzip");
        }
        output
    }

    fn pure_nbc() -> Vec<u8> {
        CodeModule::new("bundle-test")
            .to_nbc(None)
            .expect("encode nbc")
    }

    #[test]
    fn parses_current_deploy_bundle_and_binds_package_identity() {
        let bytes = make_bundle(vec![(".nula/dist/app.nbc", pure_nbc())]);
        let bundle = DeploymentBundle::parse(&bytes).expect("parse bundle");
        assert_eq!(bundle.package_manifest.package.name, "app");
        assert_eq!(bundle.artifact_path, ".nula/dist/app.nbc");
        assert_eq!(
            bundle.execution_manifest.artifact_blake3,
            blake3::hash(&bundle.artifact_bytes).to_hex().to_string()
        );

        let decision = bundle.evaluate_admission(
            &AdmissionPolicy::new("tenant/default", 1),
            &AdmissionEnvironment::default(),
        );
        assert!(decision.admitted, "{:?}", decision.reasons);
    }

    #[test]
    fn rejects_artifact_name_that_does_not_match_package() {
        let bytes = make_bundle(vec![(".nula/dist/other.nbc", pure_nbc())]);
        assert!(matches!(
            DeploymentBundle::parse(&bytes),
            Err(DeploymentBundleError::ArtifactNameMismatch { .. })
        ));
    }

    #[test]
    fn rejects_multiple_compiled_artifacts() {
        let bytes = make_bundle(vec![
            (".nula/dist/app.nbc", pure_nbc()),
            (".nula/dist/other.nbc", pure_nbc()),
        ]);
        assert!(matches!(
            DeploymentBundle::parse(&bytes),
            Err(DeploymentBundleError::MultipleArtifacts)
        ));
    }

    #[test]
    fn rejects_missing_compiled_artifact() {
        let bytes = make_bundle(vec![]);
        assert!(matches!(
            DeploymentBundle::parse(&bytes),
            Err(DeploymentBundleError::MissingArtifact)
        ));
    }

    #[test]
    fn rejects_missing_package_manifest() {
        let mut output = Vec::new();
        {
            let gzip = GzEncoder::new(&mut output, Compression::default());
            let mut builder = tar::Builder::new(gzip);
            let data = pure_nbc();
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, ".nula/dist/app.nbc", data.as_slice())
                .expect("append entry");
            let gzip = builder.into_inner().expect("finish tar");
            gzip.finish().expect("finish gzip");
        }
        assert!(matches!(
            DeploymentBundle::parse(&output),
            Err(DeploymentBundleError::MissingPackageManifest)
        ));
    }

    #[test]
    fn rejects_incomplete_web_metadata() {
        let mut ir = DeploymentIr::default();
        ir.capabilities = vec![INCOMPLETE_METADATA_CAPABILITY.to_string()];
        let bytes = make_bundle(vec![
            (".nula/dist/app.nbc", pure_nbc()),
            (
                "dist/nulang-app.ir.json",
                serde_json::to_vec(&ir).expect("serialize ir"),
            ),
        ]);
        assert!(matches!(
            DeploymentBundle::parse(&bytes),
            Err(DeploymentBundleError::IncompleteDeploymentMetadata)
        ));
    }

    #[test]
    fn archive_path_validation_rejects_traversal_and_absolute_paths() {
        assert!(matches!(
            validate_archive_path(Path::new("../escape")),
            Err(DeploymentBundleError::UnsafePath { .. })
        ));
        assert!(matches!(
            validate_archive_path(Path::new("/absolute/path")),
            Err(DeploymentBundleError::UnsafePath { .. })
        ));
        assert!(validate_archive_path(Path::new(".nula/dist/app.nbc")).is_ok());
    }

    #[test]
    fn rejects_invalid_nbc_payload() {
        let bytes = make_bundle(vec![(".nula/dist/app.nbc", b"not-nbc".to_vec())]);
        assert!(matches!(
            DeploymentBundle::parse(&bytes),
            Err(DeploymentBundleError::InvalidArtifact(_))
        ));
    }

    #[test]
    fn web_ir_round_trips_when_complete() {
        let ir = DeploymentIr::default();
        let bytes = make_bundle(vec![
            (".nula/dist/app.nbc", pure_nbc()),
            (
                "dist/nulang-app.ir.json",
                serde_json::to_vec(&ir).expect("serialize ir"),
            ),
        ]);
        let bundle = DeploymentBundle::parse(&bytes).expect("parse bundle");
        assert!(bundle.web_ir.is_some());
    }
}
