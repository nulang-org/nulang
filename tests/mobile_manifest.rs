use nulang::format::constants::{BYTECODE_VERSION, LANGUAGE_VERSION_STR};
use nulang::mobile::{
    MobileBuildManifest, MOBILE_FFI_ABI, MOBILE_MESSAGE_PROTOCOL, MOBILE_UI_PROTOCOL,
};

#[test]
fn public_mobile_manifest_contract_is_stable_and_verifiable() {
    let artifact = b"frozen-nbc-fixture";
    let capabilities = vec!["os".to_string()];
    let manifest = MobileBuildManifest::new("field-app", "app.nbc", artifact, &capabilities);

    assert_eq!(manifest.bytecode_format_version, BYTECODE_VERSION);
    assert_eq!(manifest.language_version, LANGUAGE_VERSION_STR);
    assert_eq!(manifest.ffi_abi, MOBILE_FFI_ABI);
    assert_eq!(MOBILE_UI_PROTOCOL, nulang_ui_protocol::UI_PROTOCOL_VERSION);
    assert_eq!(
        MOBILE_MESSAGE_PROTOCOL,
        nulang_ui_protocol::UI_MESSAGE_PROTOCOL_VERSION
    );
    assert_eq!(
        manifest.ui_protocol,
        nulang_ui_protocol::UI_PROTOCOL_VERSION
    );
    assert_eq!(
        manifest.message_protocol,
        nulang_ui_protocol::UI_MESSAGE_PROTOCOL_VERSION
    );
    assert_eq!(manifest.declared_capabilities, capabilities);
    assert_eq!(manifest.ios.execution, "interpreter");
    assert_eq!(manifest.android.execution, "interpreter");
    assert!(manifest.verify_artifact(artifact));
    assert!(!manifest.verify_artifact(b"modified"));

    let json = manifest.to_json().expect("serialize mobile manifest");
    let decoded: MobileBuildManifest =
        serde_json::from_str(&json).expect("decode public mobile manifest");
    assert_eq!(decoded, manifest);
}
