use nulang::semantic_abi::{value_layout_manifest, value_layout_manifest_json_pretty};

#[test]
fn value_layout_manifest_is_derived_from_canonical_core_constants() {
    let manifest = value_layout_manifest();

    assert_eq!(manifest.schema_version, 1);
    assert_eq!(manifest.word_bits, 64);
    assert_eq!(manifest.tag_bits, 16);
    assert_eq!(manifest.payload_bits, 48);
    assert_eq!(manifest.tag_shift, nulang::value_layout::TAG_SHIFT);
    assert_eq!(manifest.tag_mask, nulang::value_layout::TAG_MASK);
    assert_eq!(manifest.payload_mask, nulang::value_layout::PAYLOAD_MASK);
    assert_eq!(manifest.sign_bit, nulang::value_layout::SIGN_BIT);
    assert_eq!(
        manifest.canonical_nan_bits,
        nulang::value_layout::CANONICAL_NAN_BITS
    );

    assert_eq!(
        manifest.tags.get("object"),
        Some(&nulang::value_layout::TAG_OBJECT)
    );
    assert_eq!(
        manifest.tags.get("int"),
        Some(&nulang::value_layout::TAG_INT)
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
    assert_eq!(
        parsed["tags"]["object"],
        serde_json::json!(nulang::value_layout::TAG_OBJECT)
    );
}
