//! Regression tests for the JSONL parsing trust boundary.
//! Duplicate object keys must be rejected before serde_json::Value collapses
//! them, including nested maps and escaped-equivalent field names.

use nulang::stream_json_schema::{JsonField, JsonSchema, TypedJsonl, TypedJsonlError};
use nulang::stream_records::{JsonlError, JsonlRecords};
use nulang::stream_schema::ColumnType;
use serde_json::json;
use std::io::Cursor;

fn read_one(line: &str) -> Result<serde_json::Value, JsonlError> {
    let mut records = JsonlRecords::new(Cursor::new(line.as_bytes()), 1024).unwrap();
    records.next().expect("test contains one record")
}

fn expect_duplicate_error(line: &str) {
    let result = read_one(line);
    match result {
        Err(JsonlError::Parse(err)) => {
            assert!(
                err.to_string().contains("duplicate object key"),
                "unexpected JSON parser error: {err}"
            );
        }
        other => panic!("expected duplicate-key parsing error, got {other:?}"),
    }
}

#[test]
fn rejects_duplicate_keys_in_root_object() {
    expect_duplicate_error("{\"id\":1,\"id\":2}\n");
}

#[test]
fn rejects_duplicate_keys_in_nested_objects() {
    expect_duplicate_error("{\"outer\":{\"id\":1,\"id\":2}}\n");
}

#[test]
fn rejects_duplicate_keys_inside_objects_nested_in_arrays() {
    expect_duplicate_error("{\"items\":[{\"id\":1,\"id\":2}]}\n");
}

#[test]
fn rejects_escaped_equivalent_member_names() {
    expect_duplicate_error("{\"key\":1,\"k\\u0065y\":2}\n");
    expect_duplicate_error("{\"a\":1,\"\\u0061\":2}\n");
}

#[test]
fn rejects_surrogate_pair_equivalent_member_names() {
    expect_duplicate_error("{\"😀\":1,\"\\uD83D\\uDE00\":2}\n");
}

#[test]
fn same_name_in_distinct_objects_is_not_a_duplicate() {
    assert_eq!(
        read_one("{\"left\":{\"id\":1},\"right\":{\"id\":2}}\n").unwrap(),
        json!({"left":{"id":1},"right":{"id":2}})
    );
}

#[test]
fn distinct_keys_and_regular_jsonl_scalars_still_decode() {
    assert_eq!(read_one("{\"a\":1,\"b\":2}\n").unwrap(), json!({"a":1,"b":2}));
    assert_eq!(read_one("[true,false]\n").unwrap(), json!([true, false]));
    assert_eq!(read_one("null\n").unwrap(), json!(null));
    assert_eq!(read_one("\"hello\"\n").unwrap(), json!("hello"));
    assert_eq!(read_one("-42\n").unwrap(), json!(-42));
    assert_eq!(read_one("1.5\n").unwrap(), json!(1.5));
}

#[test]
fn fails_closed_after_duplicate_without_resuming_next_record() {
    let mut reader = JsonlRecords::new(
        Cursor::new(b"{\"id\":1}\n{\"id\":1,\"id\":2}\n{\"id\":3}\n"),
        128,
    )
    .unwrap();
    assert_eq!(reader.next().unwrap().unwrap(), json!({"id":1}));
    assert!(matches!(reader.next().unwrap(), Err(JsonlError::Parse(_))));
    assert!(reader.next().is_none());
}

#[test]
fn schema_adapter_propagates_duplicate_parse_error_without_emitting_partial_row() {
    let schema = JsonSchema::new(vec![JsonField::new("id", ColumnType::Integer)]).unwrap();
    let source = JsonlRecords::new(
        Cursor::new(b"{\"id\":1,\"id\":2}\n{\"id\":3}\n"),
        128,
    )
    .unwrap();
    let mut typed = TypedJsonl::new(source, schema);
    assert!(matches!(
        typed.next().unwrap(),
        Err(TypedJsonlError::Source(JsonlError::Parse(_)))
    ));
    assert!(typed.next().is_none());
}

#[test]
fn trailing_json_content_is_still_rejected() {
    assert!(matches!(
        read_one("{\"a\":1} {\"b\":2}\n"),
        Err(JsonlError::Parse(_))
    ));
}

#[test]
fn byte_budget_still_precedes_duplicate_key_parsing() {
    let mut reader = JsonlRecords::new(
        Cursor::new(b"{\"id\":1,\"id\":2}\n"),
        8,
    )
    .unwrap();
    assert!(matches!(reader.next().unwrap(), Err(JsonlError::Io(_))));
    assert!(reader.next().is_none());
}
