use nulang::semantic_abi::{
    semantic_abi_manifest, semantic_abi_manifest_json_pretty, value_layout_manifest,
    value_layout_manifest_json_pretty,
};

#[test]
fn value_layout_manifest_is_derived_from_canonical_core_constants() {
    let manifest = value_layout_manifest();

    assert_eq!(manifest.schema_version, 1);
    assert_eq!(
        manifest.layout_version,
        nulang::format::constants::VALUE_LAYOUT_VERSION
    );
    assert_eq!(manifest.word_bits, u64::BITS as u8);
    assert_eq!(manifest.payload_bits, nulang::value_layout::TAG_SHIFT as u8);
    assert_eq!(
        manifest.tag_bits,
        (u64::BITS - nulang::value_layout::TAG_SHIFT) as u8
    );
    assert_eq!(manifest.tag_shift, nulang::value_layout::TAG_SHIFT);
    assert_eq!(manifest.tag_mask, "0xffff000000000000");
    assert_eq!(manifest.payload_mask, "0x0000ffffffffffff");
    assert_eq!(manifest.sign_bit, "0x0000800000000000");
    assert_eq!(manifest.canonical_nan_bits, "0xfff8000000000001");

    assert_eq!(
        manifest.tags.get("object").map(String::as_str),
        Some("0x7ff5000000000000")
    );
    assert_eq!(
        manifest.tags.get("int").map(String::as_str),
        Some("0x7ffb000000000000")
    );
}

#[test]
fn value_layout_manifest_json_is_stable_and_machine_readable() {
    let json = value_layout_manifest_json_pretty().unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

    assert_eq!(parsed["schema_version"], 1);
    assert_eq!(
        parsed["layout_version"],
        nulang::format::constants::VALUE_LAYOUT_VERSION
    );
    assert_eq!(parsed["word_bits"], u64::BITS);
    assert_eq!(
        parsed["tag_bits"],
        u64::BITS - nulang::value_layout::TAG_SHIFT
    );
    assert_eq!(parsed["payload_bits"], nulang::value_layout::TAG_SHIFT);
    assert_eq!(parsed["float_encoding"], "raw-ieee754-with-canonical-nan");
    assert_eq!(parsed["tag_mask"], "0xffff000000000000");
    assert_eq!(parsed["canonical_nan_bits"], "0xfff8000000000001");
    assert_eq!(parsed["tags"]["object"], "0x7ff5000000000000");
}

#[test]
fn semantic_abi_manifest_exports_existing_versioned_runtime_contracts() {
    let manifest = semantic_abi_manifest();

    assert_eq!(manifest.schema_version, 1);
    assert_eq!(
        manifest.language_version,
        nulang::format::constants::LANGUAGE_VERSION_STR
    );
    assert_eq!(
        manifest.artifact_identity_manifest_version,
        nulang::artifact_identity::ARTIFACT_IDENTITY_MANIFEST_VERSION
    );
    assert_eq!(
        manifest.behavior_manifest_schema,
        nulang::behavior_manifest::BEHAVIOR_MANIFEST_SCHEMA
    );
    assert_eq!(
        manifest.host_effect_abi_schema,
        nulang::host_effect_abi::HOST_EFFECT_ABI_SCHEMA
    );
    assert_eq!(manifest.value_layout, value_layout_manifest());
}

#[test]
fn full_semantic_abi_json_nests_the_value_layout_contract() {
    let json = semantic_abi_manifest_json_pretty().unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

    assert_eq!(parsed["schema_version"], 1);
    assert_eq!(
        parsed["behavior_manifest_schema"],
        nulang::behavior_manifest::BEHAVIOR_MANIFEST_SCHEMA
    );
    assert_eq!(
        parsed["host_effect_abi_schema"],
        nulang::host_effect_abi::HOST_EFFECT_ABI_SCHEMA
    );
    assert_eq!(
        parsed["value_layout"]["layout_version"],
        nulang::format::constants::VALUE_LAYOUT_VERSION
    );
    assert_eq!(
        parsed["value_layout"]["tags"]["object"],
        "0x7ff5000000000000"
    );
}
