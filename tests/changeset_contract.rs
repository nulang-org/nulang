use nulang::changeset::{Changeset, FieldSpec, FieldType};
use serde_json::json;

#[test]
fn changeset_only_casts_explicit_fields_and_parses_common_form_values() {
    let changes = Changeset::cast(
        &json!({"order_id":"42", "email":" user@example.com ", "admin":true}),
        &[
            FieldSpec::required("order_id", FieldType::Integer),
            FieldSpec::optional("email", FieldType::String),
        ],
    );
    assert!(changes.is_valid(), "{:?}", changes.errors());
    let values = changes.into_result().unwrap();
    assert_eq!(values["order_id"], json!(42));
    assert_eq!(values["email"], json!(" user@example.com "));
    assert!(!values.contains_key("admin"));
}

#[test]
fn changeset_reports_required_cast_and_range_errors_without_throwing() {
    let changes = Changeset::cast(
        &json!({"total":"oops"}),
        &[
            FieldSpec::required("total", FieldType::Integer),
            FieldSpec::required("customer_id", FieldType::String),
        ],
    )
    .validate_min("total", 1);
    assert!(!changes.is_valid());
    assert!(changes
        .errors()
        .iter()
        .any(|e| e.field == "total" && e.code == "cast"));
    assert!(changes
        .errors()
        .iter()
        .any(|e| e.field == "customer_id" && e.code == "required"));
}

#[test]
fn changeset_rejects_non_object_input_and_negative_business_amount() {
    let bad = Changeset::cast(
        &json!([]),
        &[FieldSpec::required("total", FieldType::Integer)],
    );
    assert!(!bad.is_valid());

    let bad_total = Changeset::cast(
        &json!({"total":-2}),
        &[FieldSpec::required("total", FieldType::Integer)],
    )
    .validate_min("total", 0);
    assert!(bad_total
        .errors()
        .iter()
        .any(|e| e.field == "total" && e.code == "min"));
}

#[test]
fn required_text_rejects_blank_input_and_integer_cast_checks_range() {
    let changes = Changeset::cast(
        &json!({"email":"   ", "order_id":18446744073709551615u64}),
        &[
            FieldSpec::required("email", FieldType::String),
            FieldSpec::required("order_id", FieldType::Integer),
        ],
    );
    assert!(changes
        .errors()
        .iter()
        .any(|e| e.field == "email" && e.code == "required"));
    assert!(changes
        .errors()
        .iter()
        .any(|e| e.field == "order_id" && e.code == "cast"));
}
