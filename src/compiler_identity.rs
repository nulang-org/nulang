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
use crate::semantic_schema::{
    actor_definition_semantic_ids_for_typed_program, semantic_id_for_typed_program,
};

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

/// Compile one typed HIR/MIR program to bytecode and attach its proven
/// backend-independent semantic identity as an in-memory sidecar.
///
/// Low-level `mir_codegen::compile_mir` intentionally remains available for
/// tests/fuzzers/backend work and produces an unproven `CodeModule` with no
/// semantic identity. Durable/runtime entry points should use this function
/// when typed HIR is available.
pub fn compile_typed_bytecode<D>(
    hir: &hir::Module,
    mir: &mut mir::Module,
    dependency_semantic_ids: D,
    name: &str,
) -> crate::types::NuResult<crate::bytecode::CodeModule>
where
    D: IntoIterator<Item = SemanticId>,
{
    let dependencies: Vec<_> = dependency_semantic_ids.into_iter().collect();
    let semantic_id = semantic_id_for_typed_program(hir, mir, dependencies.iter().copied())
        .map_err(|error| crate::types::NuError::VMError {
            msg: format!("cannot derive canonical semantic identity: {error}"),
            span: crate::types::Span::default(),
        })?;
    let actor_semantic_ids =
        actor_definition_semantic_ids_for_typed_program(hir, mir, dependencies.iter().copied())
            .map_err(|error| crate::types::NuError::VMError {
                msg: format!("cannot derive actor semantic identities: {error}"),
                span: crate::types::Span::default(),
            })?;
    let mut module = crate::mir_codegen::compile_mir(mir, name)?;
    module.semantic_id = Some(semantic_id);
    module.actor_semantic_ids = actor_semantic_ids
        .into_iter()
        .map(|(_, semantic_id)| semantic_id)
        .collect();
    Ok(module)
}

/// Compile typed bytecode and attach complete artifact provenance.
///
/// This is the preferred boundary for durable/package artifacts: the returned
/// module carries whole-program semantic identity, per-definition semantic
/// identities, and the exact compiler/backend-specific `ArtifactId` derived
/// by the accompanying manifest. Frozen NBC v1 still omits these sidecars;
/// `crate::artifact_store` persists and restores them externally.
pub fn compile_typed_bytecode_with_artifact_identity<D, I, S>(
    source_bytes: Option<&[u8]>,
    hir: &hir::Module,
    mir: &mut mir::Module,
    dependency_semantic_ids: D,
    name: &str,
    compiler_version: &str,
    target: &str,
    abi: &str,
    backend: &str,
    flags: I,
) -> crate::types::NuResult<(
    crate::bytecode::CodeModule,
    ArtifactIdentityManifest,
)>
where
    D: IntoIterator<Item = SemanticId>,
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let dependencies: Vec<_> = dependency_semantic_ids.into_iter().collect();
    let mut module =
        compile_typed_bytecode(hir, mir, dependencies.iter().copied(), name)?;
    let semantic_id = module.semantic_id.ok_or_else(|| crate::types::NuError::VMError {
        msg: "typed compilation produced no semantic identity".to_string(),
        span: crate::types::Span::default(),
    })?;
    let manifest = ArtifactIdentityManifest::new(
        source_bytes.map(SourceId::from_bytes),
        semantic_id,
        compiler_version,
        target,
        abi,
        backend,
        flags,
    );
    module.artifact_id = Some(manifest.artifact_id());
    Ok((module, manifest))
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
    fn typed_bytecode_carries_semantic_identity_but_raw_codegen_does_not() {
        let (hir, mut typed_mir) = empty_program();
        let mut raw_mir = typed_mir.clone();

        let typed = compile_typed_bytecode(&hir, &mut typed_mir, [], "typed").unwrap();
        let raw = crate::mir_codegen::compile_mir(&mut raw_mir, "raw").unwrap();

        assert!(typed.semantic_id.is_some());
        assert!(raw.semantic_id.is_none());
        assert!(typed.actor_semantic_ids.is_empty());
        assert!(raw.actor_semantic_ids.is_empty());
    }

    #[test]
    fn typed_artifact_compile_attaches_manifest_artifact_id() {
        let (hir, mut mir) = empty_program();
        let (module, manifest) = compile_typed_bytecode_with_artifact_identity(
            Some(b"fn main() { 42 }"),
            &hir,
            &mut mir,
            [],
            "typed-artifact",
            "nulangc-test",
            "nulang-vm-v1",
            "nbc-v1",
            "bytecode",
            ["opt=0"],
        )
        .unwrap();

        assert_eq!(module.semantic_id, Some(manifest.semantic_id()));
        assert_eq!(module.artifact_id, Some(manifest.artifact_id()));
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
