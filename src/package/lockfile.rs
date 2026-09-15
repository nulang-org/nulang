//! Reading and writing the `Nulang.lock` lockfile.
//!
//! The lockfile pins the exact source each resolved dependency was fetched
//! from, so builds are reproducible. Legacy `content_hash` remains a source
//! pin/integrity field; semantic-closure identities are additive metadata and
//! deliberately do not reinterpret that field.
//!
//! ```toml
//! version = 1
//!
//! [[package]]
//! name = "util"
//! version = "0.1.0"
//! source = "path+/home/david/projects/util"
//!
//! [[identity]]
//! name = "util"
//! version = "0.1.0"
//! source = "path+/home/david/projects/util"
//! source_id = "..."
//! semantic_id = "..."
//! ```

use std::collections::BTreeSet;
use std::path::Path;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::content_identity::{SemanticId, SourceId};
use crate::package::identity::source_id_for_package_dir;
use crate::types::{NuError, NuResult, Span};

/// Lockfile name, written next to the root package's manifest.
pub const LOCKFILE_FILE: &str = "Nulang.lock";

/// Current on-disk lockfile format version.
///
/// Identity metadata is an additive optional extension to v1. Existing v1
/// lockfiles therefore remain readable and existing package-pin semantics do
/// not change.
pub const LOCKFILE_VERSION: u32 = 1;

/// A parsed `Nulang.lock`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lockfile {
    pub version: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub package: Vec<LockedPackage>,
    /// Optional strong identities for exact package pins.
    ///
    /// These records are sidecars rather than fields on `LockedPackage` so the
    /// existing resolver and legacy lockfile writers do not silently assign a
    /// new meaning to `content_hash`. Path-source IDs can be populated from
    /// canonical package inputs independently of that legacy field.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub identity: Vec<LockedPackageIdentity>,
}

/// One pinned dependency.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockedPackage {
    pub name: String,
    pub version: String,
    /// `path+<dir>` for local dependencies, `git+<url>#<rev>` for git ones.
    pub source: String,
    /// Legacy BLAKE3 hash of the resolved source (hex).
    ///
    /// This field predates [`SourceId`] and [`SemanticId`] and retains its
    /// historical meaning. It MUST NOT be interpreted as either strong ID.
    /// Empty string means the hash was not computed (for example because the
    /// source was unavailable at lock time).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub content_hash: String,
    /// Resolved git commit (full SHA) for `git+` sources, recorded at fetch
    /// time so a later resolution can detect a moved branch/tag and re-fetch
    /// the dependency. Empty for non-git sources.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub commit: String,
}

/// Optional semantic-closure identities associated with one exact package pin.
///
/// `ArtifactId` intentionally does not live in `Nulang.lock`: compiled artifact
/// identity depends on compiler/backend/target/codegen inputs and belongs in
/// artifact metadata rather than dependency-resolution metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockedPackageIdentity {
    pub name: String,
    pub version: String,
    pub source: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub source_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub semantic_id: String,
}

impl LockedPackageIdentity {
    fn key(&self) -> (&str, &str, &str) {
        (&self.name, &self.version, &self.source)
    }

    /// Parse the optional source identity into its strong type.
    pub fn parsed_source_id(&self) -> NuResult<Option<SourceId>> {
        parse_optional_identity::<SourceId>("source_id", &self.source_id, self.key())
    }

    /// Parse the optional semantic identity into its strong type.
    pub fn parsed_semantic_id(&self) -> NuResult<Option<SemanticId>> {
        parse_optional_identity::<SemanticId>("semantic_id", &self.semantic_id, self.key())
    }
}

fn parse_optional_identity<T>(
    field: &str,
    value: &str,
    (name, version, source): (&str, &str, &str),
) -> NuResult<Option<T>>
where
    T: FromStr,
    T::Err: std::fmt::Display,
{
    if value.is_empty() {
        return Ok(None);
    }
    value
        .parse::<T>()
        .map(Some)
        .map_err(|error| NuError::PackageError {
            msg: format!(
                "invalid {field} for locked package '{name}' {version} from {source}: {error}"
            ),
            span: Span::default(),
        })
}

impl Lockfile {
    /// An empty lockfile at the current format version.
    pub fn new() -> Self {
        Lockfile {
            version: LOCKFILE_VERSION,
            package: Vec::new(),
            identity: Vec::new(),
        }
    }

    /// Attach or replace strong identity metadata for an exact package pin.
    ///
    /// This method accepts typed IDs so new callers cannot accidentally write
    /// malformed identity strings. The package must already exist in the
    /// lockfile; identity metadata never manufactures a dependency pin.
    pub fn set_package_identity(
        &mut self,
        name: &str,
        version: &str,
        source: &str,
        source_id: Option<SourceId>,
        semantic_id: Option<SemanticId>,
    ) -> NuResult<()> {
        if !self.package.iter().any(|package| {
            package.name == name && package.version == version && package.source == source
        }) {
            return Err(NuError::PackageError {
                msg: format!(
                    "cannot attach identity to unknown locked package '{name}' {version} from {source}"
                ),
                span: Span::default(),
            });
        }

        let record = LockedPackageIdentity {
            name: name.to_string(),
            version: version.to_string(),
            source: source.to_string(),
            source_id: source_id.map(|id| id.to_string()).unwrap_or_default(),
            semantic_id: semantic_id.map(|id| id.to_string()).unwrap_or_default(),
        };

        if let Some(existing) = self.identity.iter_mut().find(|identity| {
            identity.name == name && identity.version == version && identity.source == source
        }) {
            *existing = record;
        } else {
            self.identity.push(record);
        }
        Ok(())
    }

    /// Return identity metadata for an exact package pin, if present.
    pub fn package_identity(
        &self,
        name: &str,
        version: &str,
        source: &str,
    ) -> Option<&LockedPackageIdentity> {
        self.identity.iter().find(|identity| {
            identity.name == name && identity.version == version && identity.source == source
        })
    }

    /// Populate canonical source identities for path dependencies whose source
    /// directories are currently available.
    ///
    /// Existing semantic identities are retained. Missing path sources are
    /// left unchanged so an already-resolved lockfile can still be copied or
    /// rewritten on a machine where that local dependency is unavailable.
    /// If an available package cannot be canonicalized safely, the operation
    /// fails rather than writing a misleading identity.
    pub fn populate_available_source_identities(&mut self) -> NuResult<()> {
        let packages = self.package.clone();
        for package in packages {
            let Some(path) = package.source.strip_prefix("path+") else {
                continue;
            };
            let path = Path::new(path);
            if !path.exists() {
                continue;
            }

            let source_id =
                source_id_for_package_dir(path).map_err(|error| NuError::PackageError {
                    msg: format!(
                        "cannot compute source identity for locked package '{}' {} from {}: {}",
                        package.name, package.version, package.source, error
                    ),
                    span: Span::default(),
                })?;
            let semantic_id = self
                .package_identity(&package.name, &package.version, &package.source)
                .map(LockedPackageIdentity::parsed_semantic_id)
                .transpose()?
                .flatten();
            self.set_package_identity(
                &package.name,
                &package.version,
                &package.source,
                Some(source_id),
                semantic_id,
            )?;
        }
        Ok(())
    }

    /// Return a clone enriched with every source identity that can be computed
    /// from currently available path dependencies.
    pub fn with_available_source_identities(&self) -> NuResult<Self> {
        let mut enriched = self.clone();
        enriched.populate_available_source_identities()?;
        Ok(enriched)
    }

    /// Serialize to TOML text.
    pub fn to_toml(&self) -> NuResult<String> {
        self.validate_identities()?;
        toml::to_string_pretty(self).map_err(|e| NuError::PackageError {
            msg: format!("cannot serialize lockfile: {}", e),
            span: Span::default(),
        })
    }

    /// Parse lockfile TOML text.
    pub fn parse(source: &str) -> NuResult<Lockfile> {
        let lockfile: Lockfile = toml::from_str(source).map_err(|e| NuError::PackageError {
            msg: format!("invalid {}: {}", LOCKFILE_FILE, e),
            span: Span::default(),
        })?;
        if lockfile.version != LOCKFILE_VERSION {
            return Err(NuError::PackageError {
                msg: format!(
                    "unsupported {} version {} (expected {})",
                    LOCKFILE_FILE, lockfile.version, LOCKFILE_VERSION
                ),
                span: Span::default(),
            });
        }
        lockfile.validate_identities()?;
        Ok(lockfile)
    }

    fn validate_identities(&self) -> NuResult<()> {
        let mut seen = BTreeSet::new();
        for identity in &self.identity {
            let key = (
                identity.name.clone(),
                identity.version.clone(),
                identity.source.clone(),
            );
            if !seen.insert(key) {
                return Err(NuError::PackageError {
                    msg: format!(
                        "duplicate identity metadata for locked package '{}' {} from {}",
                        identity.name, identity.version, identity.source
                    ),
                    span: Span::default(),
                });
            }
            if !self.package.iter().any(|package| {
                package.name == identity.name
                    && package.version == identity.version
                    && package.source == identity.source
            }) {
                return Err(NuError::PackageError {
                    msg: format!(
                        "identity metadata refers to unknown locked package '{}' {} from {}",
                        identity.name, identity.version, identity.source
                    ),
                    span: Span::default(),
                });
            }
            identity.parsed_source_id()?;
            identity.parsed_semantic_id()?;
        }
        Ok(())
    }

    /// Write the lockfile into `dir`.
    ///
    /// Available local path dependencies are enriched with canonical
    /// [`SourceId`] sidecars immediately before serialization. This changes
    /// only additive identity metadata; legacy package pins and `content_hash`
    /// retain their existing values.
    pub fn save(&self, dir: &Path) -> NuResult<()> {
        let path = dir.join(LOCKFILE_FILE);
        let enriched = self.with_available_source_identities()?;
        std::fs::write(&path, enriched.to_toml()?).map_err(|e| NuError::PackageError {
            msg: format!("cannot write {}: {}", path.display(), e),
            span: Span::default(),
        })
    }

    /// Read the lockfile from `dir`.
    pub fn load(dir: &Path) -> NuResult<Lockfile> {
        let path = dir.join(LOCKFILE_FILE);
        let source = std::fs::read_to_string(&path).map_err(|e| NuError::PackageError {
            msg: format!("cannot read {}: {}", path.display(), e),
            span: Span::default(),
        })?;
        Self::parse(&source)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_lockfile() -> Lockfile {
        Lockfile {
            version: LOCKFILE_VERSION,
            package: vec![
                LockedPackage {
                    name: "util".to_string(),
                    version: "0.1.0".to_string(),
                    source: "path+/home/david/projects/util".to_string(),
                    content_hash: "aabbcc".to_string(),
                    commit: String::new(),
                },
                LockedPackage {
                    name: "json".to_string(),
                    version: "0.2.0".to_string(),
                    source: "git+https://github.com/example/json.nu.git#v0.2.0".to_string(),
                    content_hash: String::new(),
                    commit: "a1b2c3d4e5f67890abcdef1234567890abcdef12".to_string(),
                },
            ],
            identity: Vec::new(),
        }
    }

    #[test]
    fn test_lockfile_toml_round_trip() {
        let lockfile = sample_lockfile();
        let toml_text = lockfile.to_toml().expect("lockfile should serialize");
        let parsed = Lockfile::parse(&toml_text).expect("lockfile should re-parse");
        assert_eq!(lockfile, parsed);
    }

    #[test]
    fn test_legacy_v1_without_identity_metadata_still_parses() {
        let source = r#"
version = 1

[[package]]
name = "util"
version = "0.1.0"
source = "path+/tmp/util"
content_hash = "legacy-hash"
"#;
        let parsed = Lockfile::parse(source).expect("legacy v1 lockfile should parse");
        assert_eq!(parsed.package.len(), 1);
        assert!(parsed.identity.is_empty());
        assert_eq!(parsed.package[0].content_hash, "legacy-hash");
    }

    #[test]
    fn test_lockfile_file_round_trip() {
        let dir = std::env::temp_dir().join(format!("nulang_lockfile_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir should be created");

        let lockfile = sample_lockfile();
        lockfile.save(&dir).expect("lockfile should save");
        assert!(dir.join(LOCKFILE_FILE).exists());

        let loaded = Lockfile::load(&dir).expect("lockfile should load");
        assert_eq!(lockfile, loaded);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_save_adds_source_identity_for_available_path_dependency() {
        let root = std::env::temp_dir().join(format!(
            "nulang_lockfile_identity_save_{}",
            std::process::id()
        ));
        let dependency = root.join("util");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(dependency.join("src")).unwrap();
        std::fs::write(
            dependency.join("Nulang.toml"),
            "[package]\nname = \"util\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        std::fs::write(dependency.join("src/main.nula"), "fn main() { 42 }").unwrap();

        let mut lockfile = Lockfile::new();
        let source = format!("path+{}", dependency.display());
        lockfile.package.push(LockedPackage {
            name: "util".to_string(),
            version: "0.1.0".to_string(),
            source: source.clone(),
            content_hash: "legacy-stays-legacy".to_string(),
            commit: String::new(),
        });
        let semantic_id = SemanticId::from_canonical_bytes(b"util semantics", []);
        lockfile
            .set_package_identity("util", "0.1.0", &source, None, Some(semantic_id))
            .unwrap();

        let expected_source_id = source_id_for_package_dir(&dependency).unwrap();
        lockfile.save(&root).unwrap();
        let loaded = Lockfile::load(&root).unwrap();
        let identity = loaded
            .package_identity("util", "0.1.0", &source)
            .expect("save should populate available path source identity");
        assert_eq!(
            identity.parsed_source_id().unwrap(),
            Some(expected_source_id)
        );
        assert_eq!(identity.parsed_semantic_id().unwrap(), Some(semantic_id));
        assert_eq!(loaded.package[0].content_hash, "legacy-stays-legacy");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn test_lockfile_rejects_unknown_version() {
        let source = "version = 99\n";
        let err = Lockfile::parse(source).expect_err("future versions must be rejected");
        match err {
            NuError::PackageError { msg, .. } => assert!(msg.contains("version 99")),
            other => panic!("expected PackageError, got {:?}", other),
        }
    }

    #[test]
    fn test_lockfile_content_hash_round_trips_without_becoming_source_id() {
        let mut lockfile = Lockfile::new();
        lockfile.package.push(LockedPackage {
            name: "pinned".to_string(),
            version: "1.0.0".to_string(),
            source: "path+/tmp/pinned".to_string(),
            content_hash: "deadbeef".to_string(),
            commit: String::new(),
        });
        let toml_text = lockfile.to_toml().expect("serialize");
        assert!(toml_text.contains("content_hash"));
        assert!(!toml_text.contains("source_id"));
        let parsed = Lockfile::parse(&toml_text).expect("parse");
        assert_eq!(parsed.package[0].content_hash, "deadbeef");
        assert!(parsed.identity.is_empty());
    }

    #[test]
    fn test_typed_package_identities_round_trip_additively() {
        let mut lockfile = sample_lockfile();
        let source_id = SourceId::from_bytes(b"canonical package source bytes");
        let semantic_id = SemanticId::from_canonical_bytes(b"canonical package semantics", []);
        lockfile
            .set_package_identity(
                "util",
                "0.1.0",
                "path+/home/david/projects/util",
                Some(source_id),
                Some(semantic_id),
            )
            .unwrap();

        let text = lockfile.to_toml().unwrap();
        assert!(text.contains("[[identity]]"));
        let parsed = Lockfile::parse(&text).unwrap();
        let identity = parsed
            .package_identity("util", "0.1.0", "path+/home/david/projects/util")
            .expect("identity should round-trip");
        assert_eq!(identity.parsed_source_id().unwrap(), Some(source_id));
        assert_eq!(identity.parsed_semantic_id().unwrap(), Some(semantic_id));
        assert_eq!(parsed.package[0].content_hash, "aabbcc");
    }

    #[test]
    fn test_identity_for_unknown_package_is_rejected() {
        let source_id = SourceId::from_bytes(b"source");
        let mut lockfile = sample_lockfile();
        let err = lockfile
            .set_package_identity(
                "missing",
                "1.0.0",
                "path+/tmp/missing",
                Some(source_id),
                None,
            )
            .unwrap_err();
        match err {
            NuError::PackageError { msg, .. } => assert!(msg.contains("unknown locked package")),
            other => panic!("expected PackageError, got {other:?}"),
        }
    }

    #[test]
    fn test_malformed_persisted_identity_fails_closed() {
        let source = r#"
version = 1

[[package]]
name = "util"
version = "0.1.0"
source = "path+/tmp/util"

[[identity]]
name = "util"
version = "0.1.0"
source = "path+/tmp/util"
source_id = "not-a-source-id"
"#;
        let err = Lockfile::parse(source).expect_err("invalid identity must fail closed");
        match err {
            NuError::PackageError { msg, .. } => assert!(msg.contains("invalid source_id")),
            other => panic!("expected PackageError, got {other:?}"),
        }
    }

    #[test]
    fn test_duplicate_identity_metadata_is_rejected() {
        let source_id = SourceId::from_bytes(b"source");
        let source = format!(
            r#"
version = 1

[[package]]
name = "util"
version = "0.1.0"
source = "path+/tmp/util"

[[identity]]
name = "util"
version = "0.1.0"
source = "path+/tmp/util"
source_id = "{source_id}"

[[identity]]
name = "util"
version = "0.1.0"
source = "path+/tmp/util"
source_id = "{source_id}"
"#
        );
        let err = Lockfile::parse(&source).expect_err("duplicates must fail closed");
        match err {
            NuError::PackageError { msg, .. } => assert!(msg.contains("duplicate identity")),
            other => panic!("expected PackageError, got {other:?}"),
        }
    }
}
