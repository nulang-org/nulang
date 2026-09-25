use nulang::semantic_abi::{value_layout_manifest, value_layout_manifest_json_pretty};

#[test]
fn value_layout_manifest_is_derived_from_canonical_core_constants() {
    let manifest = value_layout_manifest();

    assert_eq!(manifest.schema_version, 1);
    assert_eq!(manifest.word_bits, 64);
    assert_eq!(manifest.tag_bits, 16);
    assert_eq!(manifest.payload_bits, 48);
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
    assert_eq!(parsed["word_bits"], 64);
    assert_eq!(parsed["tag_bits"], 16);
    assert_eq!(parsed["payload_bits"], 48);
    assert_eq!(parsed["float_encoding"], "raw-ieee754-with-canonical-nan");
    assert_eq!(parsed["tag_mask"], "0xffff000000000000");
    assert_eq!(parsed["canonical_nan_bits"], "0xfff8000000000001");
    assert_eq!(parsed["tags"]["object"], "0x7ff5000000000000");
}
