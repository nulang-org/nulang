//! Mobile artifact/host manifest contract.
//!
//! Native packagers consume the ordinary frozen `.nbc` artifact. This module
//! defines the machine-readable metadata that binds that artifact to the
//! embedding ABI and UI protocols without introducing a second compiler or VM.

pub mod action;
pub mod compiler;
pub mod ffi;
pub mod runtime;

use serde::{Deserialize, Serialize};

use crate::format::constants::{BYTECODE_VERSION, LANGUAGE_VERSION_STR};

pub const MOBILE_MANIFEST_VERSION: u32 = 1;
pub const MOBILE_FFI_ABI: &str = "nulang-embed/1";
pub const MOBILE_UI_PROTOCOL: &str = nulang_ui_protocol::UI_PROTOCOL_VERSION;
pub const MOBILE_MESSAGE_PROTOCOL: &str = nulang_ui_protocol::UI_MESSAGE_PROTOCOL_VERSION;
pub const MOBILE_DOCUMENT_CALLBACK: &str = "nulang_ui_document";
pub const MOBILE_MESSAGE_CALLBACK: &str = "nulang_ui_message";
pub const MOBILE_CALLBACK_CAPABILITY: &str = "os";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MobileRuntimeTarget {
    pub execution: String,
    pub runtime_constructor: String,
    pub library_kind: String,
}

impl MobileRuntimeTarget {
    fn interpreter(library_kind: &str) -> Self {
        Self {
            execution: "interpreter".to_string(),
            runtime_constructor: "nulang_runtime_new_interpreter".to_string(),
            library_kind: library_kind.to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MobileBuildManifest {
    pub version: u32,
    pub package: String,
    pub artifact: String,
    pub artifact_bytes: usize,
    pub artifact_blake3: String,
    pub bytecode_format_version: u32,
    pub language_version: String,
    pub ffi_abi: String,
    pub ui_protocol: String,
    pub message_protocol: String,
    pub callbacks: Vec<String>,
    pub callback_capability: String,
    pub declared_capabilities: Vec<String>,
    pub ios: MobileRuntimeTarget,
    pub android: MobileRuntimeTarget,
}

impl MobileBuildManifest {
    pub fn new(package: &str, artifact: &str, bytes: &[u8], capabilities: &[String]) -> Self {
        Self {
            version: MOBILE_MANIFEST_VERSION,
            package: package.to_string(),
            artifact: artifact.to_string(),
            artifact_bytes: bytes.len(),
            artifact_blake3: blake3::hash(bytes).to_hex().to_string(),
            bytecode_format_version: BYTECODE_VERSION,
            language_version: LANGUAGE_VERSION_STR.to_string(),
            ffi_abi: MOBILE_FFI_ABI.to_string(),
            ui_protocol: MOBILE_UI_PROTOCOL.to_string(),
            message_protocol: MOBILE_MESSAGE_PROTOCOL.to_string(),
            callbacks: vec![
                MOBILE_DOCUMENT_CALLBACK.to_string(),
                MOBILE_MESSAGE_CALLBACK.to_string(),
            ],
            callback_capability: MOBILE_CALLBACK_CAPABILITY.to_string(),
            declared_capabilities: capabilities.to_vec(),
            ios: MobileRuntimeTarget::interpreter("staticlib"),
            android: MobileRuntimeTarget::interpreter("cdylib"),
        }
    }

    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    pub fn verify_artifact(&self, bytes: &[u8]) -> bool {
        self.artifact_bytes == bytes.len()
            && self.artifact_blake3 == blake3::hash(bytes).to_hex().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_pins_protocols_formats_and_interpreter_runtime() {
        let capabilities = vec!["os".to_string(), "net".to_string()];
        let manifest = MobileBuildManifest::new(
            "mobile-app",
            "app.nbc",
            b"artifact-bytes",
            &capabilities,
        );

        assert_eq!(manifest.version, MOBILE_MANIFEST_VERSION);
        assert_eq!(manifest.artifact, "app.nbc");
        assert_eq!(manifest.artifact_bytes, 14);
        assert_eq!(manifest.bytecode_format_version, BYTECODE_VERSION);
        assert_eq!(manifest.language_version, LANGUAGE_VERSION_STR);
        assert_eq!(manifest.ffi_abi, "nulang-embed/1");
        assert_eq!(MOBILE_UI_PROTOCOL, nulang_ui_protocol::UI_PROTOCOL_VERSION);
        assert_eq!(
            MOBILE_MESSAGE_PROTOCOL,
            nulang_ui_protocol::UI_MESSAGE_PROTOCOL_VERSION
        );
        assert_eq!(manifest.ui_protocol, nulang_ui_protocol::UI_PROTOCOL_VERSION);
        assert_eq!(
            manifest.message_protocol,
            nulang_ui_protocol::UI_MESSAGE_PROTOCOL_VERSION
        );
        assert_eq!(
            manifest.callbacks,
            vec![
                "nulang_ui_document".to_string(),
                "nulang_ui_message".to_string(),
            ]
        );
        assert_eq!(manifest.callback_capability, "os");
        assert_eq!(manifest.declared_capabilities, capabilities);
        assert_eq!(manifest.ios.execution, "interpreter");
        assert_eq!(
            manifest.ios.runtime_constructor,
            "nulang_runtime_new_interpreter"
        );
        assert_eq!(manifest.ios.library_kind, "staticlib");
        assert_eq!(manifest.android.execution, "interpreter");
        assert_eq!(manifest.android.library_kind, "cdylib");
    }

    #[test]
    fn manifest_round_trips_and_verifies_artifact_integrity() {
        let bytes = b"same frozen artifact";
        let manifest = MobileBuildManifest::new("app", "app.nbc", bytes, &[]);
        let encoded = manifest.to_json().expect("serialize mobile manifest");
        let decoded: MobileBuildManifest =
            serde_json::from_str(&encoded).expect("decode mobile manifest JSON");

        assert_eq!(decoded, manifest);
        assert!(decoded.verify_artifact(bytes));
        assert!(!decoded.verify_artifact(b"tampered artifact"));
    }
}
