//! Contracts for strict typed CSV rows and explicitly byte-budgeted collection.
//! The upstream CSV/JSONL readers and PullStream remain separately testable.

use nulang::stream_budget::{collect_budgeted, BudgetError, CollectionBudget};
use nulang::stream_records::CsvRecords;
use nulang::stream_schema::{
    Column, ColumnType, CsvSchema, Scalar, SchemaError, TypedCsv, TypedCsvError,
};
use std::io::Cursor;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

fn schema() -> CsvSchema {
    CsvSchema::new(vec![
        Column::new("name", ColumnType::Text),
        Column::new("age", ColumnType::Integer),
        Column::new("active", ColumnType::Boolean),
    ])
    .unwrap()
}

#[test]
fn converts_valid_csv_rows_after_exact_header_validation() {
    let csv = CsvRecords::new(Cursor::new(b"name,age,active\nAda,42,true\nLinus,-1,false\n"), 128, 3).unwrap();
    let rows: Vec<_> = TypedCsv::new(csv, schema(), true)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(
        rows[0].cells,
        vec![Scalar::Text("Ada".into()), Scalar::Integer(42), Scalar::Boolean(true)]
    );
    assert_eq!(
        rows[1].cells,
        vec![Scalar::Text("Linus".into()), Scalar::Integer(-1), Scalar::Boolean(false)]
    );
}

#[test]
fn parses_csv_without_header_when_explicitly_disabled() {
    let csv = CsvRecords::new(Cursor::new(b"Grace,0,true\n"), 64, 3).unwrap();
    let rows: Vec<_> = TypedCsv::new(csv, schema(), false)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].cells[1], Scalar::Integer(0));
}

#[test]
fn fails_closed_on_header_mismatch_missing_header_and_empty_schema() {
    assert!(matches!(CsvSchema::new(vec![]), Err(SchemaError::InvalidSchema)));
    let csv = CsvRecords::new(Cursor::new(b"name,active,age\nAda,true,42\n"), 64, 3).unwrap();
    let mut rows = TypedCsv::new(csv, schema(), true);
    assert!(matches!(rows.next().unwrap(), Err(TypedCsvError::Schema(SchemaError::HeaderMismatch { .. }))));
    assert!(rows.next().is_none());

    let csv = CsvRecords::new(Cursor::new(b""), 64, 3).unwrap();
    let mut rows = TypedCsv::new(csv, schema(), true);
    assert!(matches!(rows.next().unwrap(), Err(TypedCsvError::Schema(SchemaError::MissingHeader))));
    assert!(rows.next().is_none());
}

#[test]
fn rejects_duplicate_or_blank_column_names() {
    assert!(matches!(
        CsvSchema::new(vec![Column::new("", ColumnType::Text)]),
        Err(SchemaError::InvalidSchema)
    ));
    assert!(matches!(
        CsvSchema::new(vec![
            Column::new("same", ColumnType::Text),
            Column::new("same", ColumnType::Integer)
        ]),
        Err(SchemaError::DuplicateColumn(_))
    ));
}

#[test]
fn rejects_wrong_row_width_and_stops() {
    let csv = CsvRecords::new(Cursor::new(b"name,age,active\nAda,42\nBob,4,true\n"), 128, 3).unwrap();
    let mut rows = TypedCsv::new(csv, schema(), true);
    assert!(matches!(rows.next().unwrap(), Err(TypedCsvError::Schema(SchemaError::WidthMismatch { row: 1, expected: 3, actual: 2 }))));
    assert!(rows.next().is_none());
}

#[test]
fn rejects_invalid_boolean_and_integer_without_coercion() {
    for bytes in [&b"Ada,10,yes\n"[..], &b"Ada,1.5,true\n"[..], &b"Ada,,false\n"[..]] {
        let csv = CsvRecords::new(Cursor::new(bytes), 128, 3).unwrap();
        let mut rows = TypedCsv::new(csv, schema(), false);
        assert!(matches!(rows.next().unwrap(), Err(TypedCsvError::Schema(SchemaError::InvalidCell { row: 1, .. }))));
        assert!(rows.next().is_none());
    }
}

#[test]
fn rejects_integer_overflow_without_panicking() {
    let csv = CsvRecords::new(Cursor::new(b"Ada,9999999999999999999999999,true\n"), 128, 3).unwrap();
    let mut rows = TypedCsv::new(csv, schema(), false);
    assert!(matches!(rows.next().unwrap(), Err(TypedCsvError::Schema(SchemaError::InvalidCell { .. }))));
}

#[test]
fn propagates_csv_io_errors_and_stops() {
    let csv = CsvRecords::new(Cursor::new(b"name,age,active\nAda,\"unclosed\n"), 128, 3).unwrap();
    let mut rows = TypedCsv::new(csv, schema(), true);
    assert!(matches!(rows.next().unwrap(), Err(TypedCsvError::Source(_))));
    assert!(rows.next().is_none());
}

#[test]
fn collected_payloads_accept_exact_byte_and_item_limits() {
    let limits = CollectionBudget::new(2, 5).unwrap();
    let stream = ["ab", "cde"].into_iter().map(Ok::<_, ()>);
    let items = collect_budgeted(stream, limits, |s| s.len()).unwrap();
    assert_eq!(items, ["ab", "cde"]);
}

#[test]
fn aggregated_byte_limit_rejects_second_item_even_when_each_fits() {
    let stream = ["abc", "def"].into_iter().map(Ok::<_, ()>);
    let result = collect_budgeted(stream, CollectionBudget::new(5, 5).unwrap(), |s| s.len());
    assert!(matches!(result, Err(BudgetError::ByteLimit { limit: 5 })));
}

#[test]
fn aggregate_item_limit_detects_first_excess_without_prefetching_further() {
    let observed = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&observed);
    let stream = (0..10).map(move |n| {
        count.fetch_add(1, Ordering::SeqCst);
        Ok::<_, ()>(n)
    });
    let result = collect_budgeted(stream, CollectionBudget::new(2, 100).unwrap(), |_| 1);
    assert!(matches!(result, Err(BudgetError::ItemLimit { limit: 2 })));
    assert_eq!(observed.load(Ordering::SeqCst), 3);
}

#[test]
fn invalid_budget_and_saturating_overflow_are_rejected() {
    assert!(CollectionBudget::new(0, 10).is_err());
    assert!(CollectionBudget::new(1, 0).is_err());
    assert!(CollectionBudget::new(1, 64 * 1024 * 1024 + 1).is_err());
    let result = collect_budgeted(
        [Ok::<_, ()>("a")],
        CollectionBudget::new(2, 10).unwrap(),
        |_| usize::MAX,
    );
    assert!(matches!(result, Err(BudgetError::ByteLimit { limit: 10 })));
}

#[test]
fn source_failure_precedes_later_items_and_returns_no_partial_collection() {
    let stream = [Ok("hello"), Err("failed"), Ok("later")].into_iter();
    let result = collect_budgeted(stream, CollectionBudget::new(4, 20).unwrap(), |s| s.len());
    assert!(matches!(result, Err(BudgetError::Source("failed"))));
}

#[test]
fn typed_csv_payload_budget_can_combine_with_stream_collection() {
    let csv = CsvRecords::new(Cursor::new(b"name,age,active\nAda,42,true\nBob,1,false\n"), 128, 3).unwrap();
    let rows = TypedCsv::new(csv, schema(), true);
    let result = collect_budgeted(rows, CollectionBudget::new(10, 10).unwrap(), |row| row.payload_bytes());
    assert!(matches!(result, Err(BudgetError::ByteLimit { limit: 10 })));
}
