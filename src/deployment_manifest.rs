//! Compiled deployment manifest for Nulang artifacts.
//!
//! The manifest is derived from the exact `.nbc` bytes that will be deployed,
//! not from source text. This gives Nulang Cloud and other deployers a stable,
//! content-addressed policy input for runtime selection and sandbox authority.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::bytecode::CodeModule;
use crate::execution_profile::ExecutionProfile;

/// Current compiled execution manifest schema.
pub const DEPLOYMENT_MANIFEST_SCHEMA_VERSION: u32 = 1;

/// Security- and scheduling-relevant metadata derived from a compiled artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompiledExecutionManifest {
    /// Manifest schema version, independent of the `.nbc` format version.
    pub schema_version: u32,
    /// BLAKE3 digest of the exact `.nbc` bytes this manifest describes.
    pub artifact_blake3: String,
    /// Stable names of runtime subsystems required by the artifact.
    pub runtime_features: Vec<String>,
    /// Fully-qualified unresolved/suspending effect operations encoded in bytecode.
    pub external_effects: Vec<String>,
    /// Root namespaces of external effects (`DB.query` -> `DB`).
    pub external_effect_roots: Vec<String>,
    /// Explicit authority tokens attached to compiled spawn sites.
    pub spawn_capabilities: Vec<String>,
    /// Native libraries named by compiled FFI declarations. Useful for runtime
    /// availability/scheduling; admission can enforce the narrower function list.
    pub ffi_libraries: Vec<String>,
    /// Exact native call authorities in canonical `library::symbol` form.
    pub ffi_functions: Vec<String>,
    /// True when the artifact can cross a host/authority boundary.
    pub reaches_host_boundary: bool,
    /// True when no durable/persistent actor runtime is required.
    pub ephemeral: bool,
    /// True when no cross-node distribution runtime is required.
    pub local_only: bool,
}

impl CompiledExecutionManifest {
    /// Decode an `.nbc` artifact and derive a manifest bound to those exact bytes.
    pub fn from_nbc_bytes(bytes: &[u8]) -> Result<Self, String> {
        let artifact = CodeModule::from_nbc(bytes).map_err(|err| err.to_string())?;
        Ok(Self::from_module_and_bytes(&artifact.module, bytes))
    }

    /// Derive a manifest when the decoded module and original artifact bytes are
    /// already available to the caller.
    pub fn from_module_and_bytes(module: &CodeModule, bytes: &[u8]) -> Self {
        let profile = ExecutionProfile::analyze(module);
        let runtime_features = profile.feature_names().map(str::to_string).collect();
        let external_effects = profile.external_effects().map(str::to_string).collect();
        let external_effect_roots = profile.external_effect_roots();

        let mut spawn_capabilities = BTreeSet::new();
        for (_, grants) in &module.spawn_capability_grants {
            spawn_capabilities.extend(grants.iter().cloned());
        }

        let mut ffi_libraries = BTreeSet::new();
        let mut ffi_functions = BTreeSet::new();
        for function in &module.foreign_functions {
            ffi_libraries.insert(function.library.clone());
            ffi_functions.insert(format!("{}::{}", function.library, function.symbol));
        }

        Self {
            schema_version: DEPLOYMENT_MANIFEST_SCHEMA_VERSION,
            artifact_blake3: blake3::hash(bytes).to_hex().to_string(),
            runtime_features,
            external_effects,
            external_effect_roots,
            spawn_capabilities: spawn_capabilities.into_iter().collect(),
            ffi_libraries: ffi_libraries.into_iter().collect(),
            ffi_functions: ffi_functions.into_iter().collect(),
            reaches_host_boundary: profile.reaches_host_boundary(),
            ephemeral: profile.is_ephemeral(),
            local_only: profile.is_local_only(),
        }
    }

    /// Serialize deterministically for sidecar artifacts and Cloud APIs.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bytecode::{Constant, FfiType, ForeignFunctionDef, Instruction, OpCode};

    fn emit_effect(module: &mut CodeModule, name: &str) {
        let idx = module.add_constant(Constant::String(name.to_string()));
        module.emit(Instruction::new3(
            OpCode::Perform,
            ((idx >> 8) & 0xff) as u8,
            (idx & 0xff) as u8,
            0,
        ));
    }

    #[test]
    fn manifest_is_bound_to_exact_artifact_bytes() {
        let module = CodeModule::new("manifest-test");
        let a = CompiledExecutionManifest::from_module_and_bytes(&module, b"artifact-a");
        let b = CompiledExecutionManifest::from_module_and_bytes(&module, b"artifact-b");
        assert_ne!(a.artifact_blake3, b.artifact_blake3);
        assert_eq!(a.schema_version, DEPLOYMENT_MANIFEST_SCHEMA_VERSION);
    }

    #[test]
    fn manifest_exposes_compiled_effects_and_spawn_authority() {
        let mut module = CodeModule::new("manifest-test");
        emit_effect(&mut module, "DB.query");
        module.spawn_capability_grants.push((
            0,
            vec![
                "Net::TcpOut(api.example.com:443)".to_string(),
                "DB::Read(customers)".to_string(),
                "Net::TcpOut(api.example.com:443)".to_string(),
            ],
        ));

        let manifest = CompiledExecutionManifest::from_module_and_bytes(&module, b"nbc");
        assert_eq!(manifest.external_effects, vec!["DB.query"]);
        assert_eq!(manifest.external_effect_roots, vec!["DB"]);
        assert_eq!(
            manifest.spawn_capabilities,
            vec!["DB::Read(customers)", "Net::TcpOut(api.example.com:443)"]
        );
        assert!(manifest.reaches_host_boundary);
    }

    #[test]
    fn manifest_exposes_exact_ffi_authority() {
        let mut module = CodeModule::new("manifest-test");
        module.foreign_functions.push(ForeignFunctionDef {
            library: "libc.so.6".to_string(),
            symbol: "getpid".to_string(),
            params: vec![],
            ret: FfiType::Int,
        });
        module.foreign_functions.push(ForeignFunctionDef {
            library: "libc.so.6".to_string(),
            symbol: "getuid".to_string(),
            params: vec![],
            ret: FfiType::Int,
        });

        let manifest = CompiledExecutionManifest::from_module_and_bytes(&module, b"nbc");
        assert_eq!(manifest.ffi_libraries, vec!["libc.so.6"]);
        assert_eq!(
            manifest.ffi_functions,
            vec!["libc.so.6::getpid", "libc.so.6::getuid"]
        );
        assert!(manifest.reaches_host_boundary);
    }

    #[test]
    fn manifest_round_trips_from_real_nbc_bytes() {
        let mut module = CodeModule::new("manifest-test");
        emit_effect(&mut module, "Net.fetch");
        let bytes = module.to_nbc(None).expect("encode nbc");

        let manifest = CompiledExecutionManifest::from_nbc_bytes(&bytes).expect("decode nbc");
        assert_eq!(manifest.external_effects, vec!["Net.fetch"]);
        assert_eq!(
            manifest.artifact_blake3,
            blake3::hash(&bytes).to_hex().to_string()
        );
    }

    #[test]
    fn manifest_json_has_stable_policy_fields() {
        let module = CodeModule::new("manifest-test");
        let manifest = CompiledExecutionManifest::from_module_and_bytes(&module, b"nbc");
        let json = manifest.to_json().expect("serialize manifest");
        assert!(json.contains("\"schema_version\""));
        assert!(json.contains("\"artifact_blake3\""));
        assert!(json.contains("\"runtime_features\""));
        assert!(json.contains("\"external_effects\""));
        assert!(json.contains("\"ffi_functions\""));
    }
}
