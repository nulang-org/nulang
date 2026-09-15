//! Compiler-facing identity assembly for typed Nulang programs.
//!
//! This is the integration boundary between exact source identity, canonical
//! typed semantics, and backend/compiler-specific artifact identity. Keeping the
//! three layers explicit avoids accidental cache invalidation from formatting
//! changes while still making source provenance available to tooling.

use crate::artifact_identity::ArtifactIdentityManifest;
use crate::content_identity::{SemanticId, SourceId};
use crate::hir;
use crate::mir;
use crate::semantic_identity::SemanticIdentityError;
use crate::semantic_schema::semantic_id_for_typed_program;

/// Derive a complete artifact identity manifest from one typed/lowered program.
///
/// - `source_bytes` are optional exact input bytes used only for [`SourceId`].
/// - semantic identity comes from typed HIR + backend-independent MIR.
/// - artifact identity comes from semantic identity + compiler/backend inputs.
///
/// This function deliberately does not serialize or embed the manifest into the
/// frozen `.nbc` v1 format. Callers can persist the additive manifest separately
/// until a versioned artifact-format migration owns that embedding.
pub fn artifact_identity_for_typed_program<D, I, S>(
    source_bytes: Option<&[u8]>,
    hir: &hir::Module,
    mir: &mir::Module,
    dependency_semantic_ids: D,
    compiler_version: &str,
    target: &str,
    abi: &str,
    backend: &str,
    flags: I,
) -> Result<ArtifactIdentityManifest, SemanticIdentityError>
where
    D: IntoIterator<Item = SemanticId>,
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let source_id = source_bytes.map(SourceId::from_bytes);
    let semantic_id = semantic_id_for_typed_program(hir, mir, dependency_semantic_ids)?;

    Ok(ArtifactIdentityManifest::new(
        source_id,
        semantic_id,
        compiler_version,
        target,
        abi,
        backend,
        flags,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_program() -> (hir::Module, mir::Module) {
        (
            hir::Module {
                name: "identity-test".to_string(),
                decls: Vec::new(),
            },
            mir::Module::new("identity-test"),
        )
    }

    #[test]
    fn source_only_changes_do_not_change_semantic_or_artifact_identity() {
        let (hir, mir) = empty_program();
        let first = artifact_identity_for_typed_program(
            Some(b"fn main() { 1 }"),
            &hir,
            &mir,
            [],
            "nulangc-test",
            "x86_64-unknown-linux-gnu",
            "nulang-abi-v1",
            "bytecode",
            ["opt=0"],
        )
        .unwrap();
        let reformatted = artifact_identity_for_typed_program(
            Some(b"fn main() {\n    1\n}"),
            &hir,
            &mir,
            [],
            "nulangc-test",
            "x86_64-unknown-linux-gnu",
            "nulang-abi-v1",
            "bytecode",
            ["opt=0"],
        )
        .unwrap();

        assert_ne!(first.source_id(), reformatted.source_id());
        assert_eq!(first.semantic_id(), reformatted.semantic_id());
        assert_eq!(first.artifact_id(), reformatted.artifact_id());
    }

    #[test]
    fn backend_configuration_changes_artifact_but_not_semantic_identity() {
        let (hir, mir) = empty_program();
        let native = artifact_identity_for_typed_program(
            None,
            &hir,
            &mir,
            [],
            "nulangc-test",
            "x86_64-unknown-linux-gnu",
            "nulang-abi-v1",
            "native",
            ["opt=3"],
        )
        .unwrap();
        let wasm = artifact_identity_for_typed_program(
            None,
            &hir,
            &mir,
            [],
            "nulangc-test",
            "wasm32-wasip2",
            "nulang-abi-v1",
            "wasm",
            ["opt=3"],
        )
        .unwrap();

        assert_eq!(native.semantic_id(), wasm.semantic_id());
        assert_ne!(native.artifact_id(), wasm.artifact_id());
    }
}
