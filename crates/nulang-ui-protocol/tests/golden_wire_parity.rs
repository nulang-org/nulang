//! Shared, language-neutral Rust/Kotlin v1 wire compatibility fixtures.
//! These assert acceptance/rejection and full-width semantics, not JSON byte formatting.

use nulang_ui_protocol::{Revision, RuntimeToHostMessage, WireValue};

fn read(name: &str) -> &'static str {
    match name {
        "snapshot_valid" => {
            include_str!("../../../tests/fixtures/ui_protocol/v1/snapshot_valid.json")
        }
        "patch_valid" => include_str!("../../../tests/fixtures/ui_protocol/v1/patch_valid.json"),
        "snapshot_duplicate_event" => {
            include_str!("../../../tests/fixtures/ui_protocol/v1/snapshot_duplicate_event.json")
        }
        "snapshot_null_actions" => {
            include_str!("../../../tests/fixtures/ui_protocol/v1/snapshot_null_actions.json")
        }
        "snapshot_negative_zero" => {
            include_str!("../../../tests/fixtures/ui_protocol/v1/snapshot_negative_zero.json")
        }
        "patch_nonincreasing" => {
            include_str!("../../../tests/fixtures/ui_protocol/v1/patch_nonincreasing.json")
        }
        "snapshot_duplicate_key" => {
            include_str!("../../../tests/fixtures/ui_protocol/v1/snapshot_duplicate_key.json")
        }
        "snapshot_invalid_float" => {
            include_str!("../../../tests/fixtures/ui_protocol/v1/snapshot_invalid_float.json")
        }
        _ => panic!("unknown golden fixture: {name}"),
    }
}

// Rust's externally validated decode is deliberately explicit here: this
// standalone PR can be compiled before the stacked canonical decoder #2102.
fn validated(message: &str) -> Result<RuntimeToHostMessage, String> {
    let decoded: RuntimeToHostMessage =
        serde_json::from_str(message).map_err(|error| error.to_string())?;
    decoded.validate().map_err(|error| error.to_string())?;
    Ok(decoded)
}

#[test]
fn test_shared_snapshot_accepts_lossless_values_and_action_events() {
    let decoded = validated(read("snapshot_valid")).expect("valid fixture must pass");
    let RuntimeToHostMessage::Snapshot { document, .. } = decoded else {
        panic!("expected snapshot")
    };
    assert_eq!(document.revision, Revision(18_446_744_073_709_551_613));
    let button = document
        .nodes
        .iter()
        .find(|node| node.id.as_str() == "button")
        .unwrap();
    assert_eq!(button.actions.len(), 2);
    assert_eq!(button.actions[0].event, "press");
    assert_eq!(button.actions[1].event, "focus");
    assert_eq!(
        button.properties.get("count"),
        Some(&WireValue::from(i64::MIN))
    );
    assert_eq!(
        button.properties.get("top"),
        Some(&WireValue::from(i64::MAX))
    );
    assert_eq!(
        button.properties.get("bits"),
        Some(&WireValue::F64(nulang_ui_protocol::F64Bits(
            0x7ff8_0000_0000_0042
        )))
    );
    assert_eq!(
        button.properties.get("raw"),
        Some(&WireValue::Bytes(vec![0, 1, 127, 128, 255]))
    );
    assert_eq!(
        button.properties.get("title"),
        Some(&WireValue::from("héllo 🌍"))
    );
}

#[test]
fn test_shared_patch_applies_atomically_and_preserves_revision() {
    let RuntimeToHostMessage::Snapshot { mut document, .. } =
        validated(read("snapshot_valid")).unwrap()
    else {
        panic!("expected snapshot")
    };
    let RuntimeToHostMessage::Patch { patch, .. } = validated(read("patch_valid")).unwrap() else {
        panic!("expected patch")
    };
    document.apply_patch(&patch).unwrap();
    assert_eq!(document.revision, Revision(18_446_744_073_709_551_614));
    let button = document
        .nodes
        .iter()
        .find(|node| node.id.as_str() == "button")
        .unwrap();
    assert_eq!(
        button.properties.get("title"),
        Some(&WireValue::from("saved"))
    );
    let before = document.clone();
    assert!(
        document.apply_patch(&patch).is_err(),
        "replayed patch rejected"
    );
    assert_eq!(document, before, "failed replay preserves state");
}

#[test]
fn test_shared_invalid_wire_cases_fail_closed() {
    for name in [
        "snapshot_duplicate_event",
        "snapshot_null_actions",
        "snapshot_negative_zero",
        "patch_nonincreasing",
        "snapshot_duplicate_key",
        "snapshot_invalid_float",
    ] {
        assert!(
            validated(read(name)).is_err(),
            "Rust unexpectedly accepted {name}"
        );
    }
}
