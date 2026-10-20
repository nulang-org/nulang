//! Strict typed JSONL object validation on the experimental Rust host stream.
//! Record parsing and aggregate payload limits belong to prerequisite PRs.

use nulang::stream_budget::{collect_budgeted, BudgetError, CollectionBudget};
use nulang::stream_json_schema::{
    JsonField, JsonRow, JsonSchema, JsonSchemaError, TypedJsonl, TypedJsonlError,
};
use nulang::stream_records::JsonlRecords;
use nulang::stream_schema::{ColumnType, Scalar};
use std::io::Cursor;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

fn schema() -> JsonSchema {
    JsonSchema::new(vec![
        JsonField::new("name", ColumnType::Text),
        JsonField::new("age", ColumnType::Integer),
        JsonField::new("active", ColumnType::Boolean),
    ])
    .unwrap()
}

#[test]
fn typed_json_objects_preserve_schema_order_not_source_key_order() {
    let source = b"{\"active\":true,\"age\":42,\"name\":\"Ada\"}\n{\"name\":\"Ben\",\"age\":-7,\"active\":false}\n";
    let reader = JsonlRecords::new(Cursor::new(source), 128).unwrap();
    let rows = TypedJsonl::new(reader, schema())
        .collect::<Result<Vec<JsonRow>, _>>()
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(
        rows[0].fields,
        vec![
            ("name".into(), Scalar::Text("Ada".into())),
            ("age".into(), Scalar::Integer(42)),
            ("active".into(), Scalar::Boolean(true)),
        ]
    );
    assert_eq!(rows[1].fields[1].1, Scalar::Integer(-7));
}

#[test]
fn schema_rejects_empty_duplicate_blank_and_overlong_keys() {
    assert!(matches!(JsonSchema::new(vec![]), Err(JsonSchemaError::InvalidSchema)));
    assert!(matches!(
        JsonSchema::new(vec![JsonField::new(" ", ColumnType::Text)]),
        Err(JsonSchemaError::InvalidSchema)
    ));
    assert!(matches!(
        JsonSchema::new(vec![
            JsonField::new("id", ColumnType::Text),
            JsonField::new("id", ColumnType::Integer),
        ]),
        Err(JsonSchemaError::DuplicateField(_))
    ));
    assert!(matches!(
        JsonSchema::new(vec![JsonField::new("x".repeat(257), ColumnType::Text)]),
        Err(JsonSchemaError::InvalidSchema)
    ));
}

#[test]
fn rejects_nonobject_top_level_arrays_scalars_and_null() {
    for line in [b"[]\n".as_slice(), b"42\n", b"null\n", b"\"name\"\n"] {
        let reader = JsonlRecords::new(Cursor::new(line), 64).unwrap();
        let mut typed = TypedJsonl::new(reader, schema());
        assert!(matches!(
            typed.next().unwrap(),
            Err(TypedJsonlError::Schema(JsonSchemaError::NotObject { row: 1 }))
        ));
        assert!(typed.next().is_none());
    }
}

#[test]
fn rejects_missing_required_field_with_row_and_column_diagnostic() {
    let reader = JsonlRecords::new(
        Cursor::new(b"{\"name\":\"Ada\",\"active\":true}\n"),
        64,
    )
    .unwrap();
    let mut typed = TypedJsonl::new(reader, schema());
    assert!(matches!(
        typed.next().unwrap(),
        Err(TypedJsonlError::Schema(JsonSchemaError::MissingField {
            row: 1,
            column: 1
        }))
    ));
    assert!(typed.next().is_none());
}

#[test]
fn rejects_unexpected_object_fields_without_echoing_values() {
    let reader = JsonlRecords::new(
        Cursor::new(b"{\"name\":\"Ada\",\"age\":1,\"active\":true,\"extra\":99}\n"),
        128,
    )
    .unwrap();
    let mut typed = TypedJsonl::new(reader, schema());
    assert!(matches!(
        typed.next().unwrap(),
        Err(TypedJsonlError::Schema(JsonSchemaError::UnexpectedField { row: 1 }))
    ));
    assert!(typed.next().is_none());
}

#[test]
fn rejects_type_mismatches_without_coercion() {
    let bad_rows = [
        "{\"name\":null,\"age\":1,\"active\":true}\n",
        "{\"name\":\"Ada\",\"age\":1.0,\"active\":true}\n",
        "{\"name\":\"Ada\",\"age\":\"1\",\"active\":true}\n",
        "{\"name\":\"Ada\",\"age\":1,\"active\":\"true\"}\n",
        "{\"name\":\"Ada\",\"age\":9223372036854775808,\"active\":true}\n",
    ];
    for line in bad_rows {
        let reader = JsonlRecords::new(Cursor::new(line.as_bytes()), 256).unwrap();
        let mut typed = TypedJsonl::new(reader, schema());
        assert!(matches!(
            typed.next().unwrap(),
            Err(TypedJsonlError::Schema(JsonSchemaError::InvalidFieldType {
                row: 1,
                ..
            }))
        ));
        assert!(typed.next().is_none());
    }
}

#[test]
fn errors_report_correct_record_number_and_stop_before_later_rows() {
    let reader = JsonlRecords::new(
        Cursor::new(
            b"{\"name\":\"Ada\",\"age\":1,\"active\":true}\n{\"name\":\"Bob\",\"age\":1,\"active\":null}\n{\"name\":\"Cy\",\"age\":2,\"active\":true}\n",
        ),
        128,
    )
    .unwrap();
    let mut typed = TypedJsonl::new(reader, schema());
    assert!(typed.next().unwrap().is_ok());
    assert!(matches!(
        typed.next().unwrap(),
        Err(TypedJsonlError::Schema(JsonSchemaError::InvalidFieldType {
            row: 2,
            column: 2
        }))
    ));
    assert!(typed.next().is_none());
}

#[test]
fn malformed_json_source_error_is_preserved_and_terminal() {
    let reader = JsonlRecords::new(
        Cursor::new(b"{\"name\":\"Ada\",\"age\":1,\"active\":true}\nnot-json\n"),
        128,
    )
    .unwrap();
    let mut typed = TypedJsonl::new(reader, schema());
    assert!(typed.next().unwrap().is_ok());
    assert!(matches!(typed.next().unwrap(), Err(TypedJsonlError::Source(_))));
    assert!(typed.next().is_none());
}

#[test]
fn lazy_stream_never_prefetches_behind_terminal_limit() {
    let polls = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&polls);
    let values = [
        serde_json::json!({"name":"Ada","age":1,"active":true}),
        serde_json::json!({"name":"Bob","age":2,"active":false}),
    ];
    let source = values.into_iter().map(move |value| {
        count.fetch_add(1, Ordering::SeqCst);
        Ok::<_, nulang::stream_records::JsonlError>(value)
    });
    let rows = TypedJsonl::new(source, schema());
    assert_eq!(polls.load(Ordering::SeqCst), 0);
    let selected: Vec<_> = rows.take(1).collect::<Result<_, _>>().unwrap();
    assert_eq!(selected.len(), 1);
    assert_eq!(polls.load(Ordering::SeqCst), 1);
}

#[test]
fn typed_json_rows_enforce_aggregate_payload_budget() {
    let source = b"{\"name\":\"Ada\",\"age\":42,\"active\":true}\n{\"name\":\"Ben\",\"age\":7,\"active\":false}\n";
    let reader = JsonlRecords::new(Cursor::new(source), 128).unwrap();
    let rows = TypedJsonl::new(reader, schema());
    let result = collect_budgeted(rows, CollectionBudget::new(2, 26).unwrap(), |row| {
        row.payload_bytes()
    });
    assert!(matches!(result, Err(BudgetError::ByteLimit { limit: 26 })));
}

#[test]
fn typed_json_schema_accepts_empty_input() {
    let reader = JsonlRecords::new(Cursor::new(b""), 128).unwrap();
    let rows = TypedJsonl::new(reader, schema())
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert!(rows.is_empty());
}
