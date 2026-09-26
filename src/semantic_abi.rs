//! Versioned, machine-readable contracts exported by the Nulang compiler/runtime.
//!
//! This module is intentionally derived from canonical implementation constants
//! rather than duplicating them in downstream consumers. Hosted runtimes such as
//! Nulang Cloud can pin these manifests alongside compiler provenance.
//!
//! 64-bit bit patterns are encoded as fixed-width hexadecimal strings so JSON
//! consumers in languages with IEEE-754-only numbers do not lose precision.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::artifact_identity::ARTIFACT_IDENTITY_MANIFEST_VERSION;
use crate::behavior_manifest::BEHAVIOR_MANIFEST_SCHEMA;
use crate::format::constants::{LANGUAGE_VERSION_STR, VALUE_LAYOUT_VERSION};
use crate::host_effect_abi::HOST_EFFECT_ABI_SCHEMA;
use crate::value_layout::{
    CANONICAL_NAN_BITS, PAYLOAD_MASK, SIGN_BIT, TAG_ACTOR, TAG_BOOL, TAG_CLOSURE, TAG_INT,
    TAG_MASK, TAG_NIL, TAG_OBJECT, TAG_PTR, TAG_SHIFT, TAG_STRING, TAG_UNIT,
};

/// Schema version for the top-level exported semantic ABI manifest.
pub const SEMANTIC_ABI_MANIFEST_VERSION: u16 = 1;

/// Schema version for the exported value-layout semantic ABI.
pub const VALUE_LAYOUT_ABI_VERSION: u16 = 1;

/// Machine-readable index of versioned contracts owned by Nulang.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SemanticAbiManifest {
    pub schema_version: u16,
    pub language_version: String,
    pub artifact_identity_manifest_version: u16,
    pub behavior_manifest_schema: String,
    pub host_effect_abi_schema: String,
    pub value_layout: ValueLayoutManifest,
}

/// Machine-readable description of Nulang's canonical tagged-word value layout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ValueLayoutManifest {
    /// Version of this JSON contract shape.
    pub schema_version: u16,
    /// Frozen runtime/artifact value-layout identity.
    pub layout_version: u32,
    pub word_bits: u8,
    pub tag_bits: u8,
    pub payload_bits: u8,
    pub tag_shift: u32,
    pub tag_mask: String,
    pub payload_mask: String,
    pub sign_bit: String,
    pub canonical_nan_bits: String,
    pub float_encoding: String,
    pub tags: BTreeMap<String, String>,
}

/// Return the current top-level semantic ABI manifest.
pub fn semantic_abi_manifest() -> SemanticAbiManifest {
    SemanticAbiManifest {
        schema_version: SEMANTIC_ABI_MANIFEST_VERSION,
        language_version: LANGUAGE_VERSION_STR.to_string(),
        artifact_identity_manifest_version: ARTIFACT_IDENTITY_MANIFEST_VERSION,
        behavior_manifest_schema: BEHAVIOR_MANIFEST_SCHEMA.to_string(),
        host_effect_abi_schema: HOST_EFFECT_ABI_SCHEMA.to_string(),
        value_layout: value_layout_manifest(),
    }
}

/// Serialize the complete semantic ABI manifest as deterministic pretty JSON.
pub fn semantic_abi_manifest_json_pretty() -> Result<String, serde_json::Error> {
    serde_json::to_string_pretty(&semantic_abi_manifest())
}

/// Return the current canonical value-layout ABI manifest.
pub fn value_layout_manifest() -> ValueLayoutManifest {
    let tags = [
        ("object", TAG_OBJECT),
        ("closure", TAG_CLOSURE),
        ("nil", TAG_NIL),
        ("unit", TAG_UNIT),
        ("bool", TAG_BOOL),
        ("int", TAG_INT),
        ("ptr", TAG_PTR),
        ("actor", TAG_ACTOR),
        ("string", TAG_STRING),
    ]
    .into_iter()
    .map(|(name, value)| (name.to_string(), hex64(value)))
    .collect();

    let word_bits = u64::BITS as u8;
    let payload_bits = TAG_SHIFT as u8;
    let tag_bits = (u64::BITS - TAG_SHIFT) as u8;

    ValueLayoutManifest {
        schema_version: VALUE_LAYOUT_ABI_VERSION,
        layout_version: VALUE_LAYOUT_VERSION,
        word_bits,
        tag_bits,
        payload_bits,
        tag_shift: TAG_SHIFT,
        tag_mask: hex64(TAG_MASK),
        payload_mask: hex64(PAYLOAD_MASK),
        sign_bit: hex64(SIGN_BIT),
        canonical_nan_bits: hex64(CANONICAL_NAN_BITS),
        float_encoding: "raw-ieee754-with-canonical-nan".to_string(),
        tags,
    }
}

/// Serialize the canonical value-layout ABI as deterministic pretty JSON.
///
/// A `BTreeMap` is used for tags so output ordering is stable across runs.
pub fn value_layout_manifest_json_pretty() -> Result<String, serde_json::Error> {
    serde_json::to_string_pretty(&value_layout_manifest())
}

fn hex64(value: u64) -> String {
    format!("0x{value:016x}")
}
