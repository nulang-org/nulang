//! Behavioral contracts for bounded incremental JSONL and RFC-4180-style CSV readers.
//! This is a Rust-host API atop the experimental pull-stream foundation.

use nulang::stream_records::{CsvRecords, JsonlError, JsonlRecords};
use nulang::stream_pull::{PullStream, StreamFailure};
use serde_json::json;
use std::io::{BufReader, Cursor, ErrorKind};

#[test]
fn jsonl_decodes_objects_arrays_and_scalars_in_source_order() {
    let input = Cursor::new(b"{\"id\":1}\n[true,false]\n42");
    let reader = JsonlRecords::new(input, 64).unwrap();
    let rows = PullStream::new(reader).try_collect_bounded(3).unwrap();
    assert_eq!(rows, vec![json!({"id": 1}), json!([true, false]), json!(42)]);
}

#[test]
fn jsonl_handles_empty_input_without_a_phantom_row() {
    let reader = JsonlRecords::new(Cursor::new(b""), 8).unwrap();
    assert!(PullStream::new(reader).try_collect_bounded(2).unwrap().is_empty());
}

#[test]
fn jsonl_reports_parse_errors_and_stops_instead_of_resynchronizing() {
    let mut reader = JsonlRecords::new(Cursor::new(b"{\"id\":1}\nnot-json\ntrue\n"), 64).unwrap();
    assert_eq!(reader.next().unwrap().unwrap(), json!({"id": 1}));
    assert!(matches!(reader.next().unwrap(), Err(JsonlError::Parse(_))));
    assert!(reader.next().is_none());
}

#[test]
fn jsonl_rejects_blank_records_and_invalid_utf8() {
    let mut blanks = JsonlRecords::new(Cursor::new(b"\ntrue\n"), 32).unwrap();
    assert!(matches!(blanks.next().unwrap(), Err(JsonlError::Parse(_))));
    assert!(blanks.next().is_none());

    let mut bytes = JsonlRecords::new(Cursor::new(&[0xff, b'\n'][..]), 32).unwrap();
    assert!(matches!(bytes.next().unwrap(), Err(JsonlError::Io(_))));
    assert!(bytes.next().is_none());
}

#[test]
fn jsonl_enforces_a_raw_record_byte_limit() {
    let mut reader = JsonlRecords::new(Cursor::new(b"123456\ntrue\n"), 6).unwrap();
    assert!(matches!(reader.next().unwrap(), Err(JsonlError::Io(_))));
    assert!(reader.next().is_none());
}

#[test]
fn csv_parses_quoted_delimiters_multiline_fields_and_escaped_quotes() {
    let input = b"name,notes\r\nAlice,\"hello, world\"\r\nBob,\"line 1\nline 2\"\nChris,\"say \"\"hello\"\"\"";
    let buffered = BufReader::with_capacity(1, Cursor::new(input));
    let reader = CsvRecords::new(buffered, 128, 3).unwrap();
    let rows = PullStream::new(reader).try_collect_bounded(4).unwrap();
    assert_eq!(rows, vec![
        vec!["name", "notes"],
        vec!["Alice", "hello, world"],
        vec!["Bob", "line 1\nline 2"],
        vec!["Chris", "say \"hello\""],
    ]);
}

#[test]
fn csv_preserves_empty_columns_trailing_column_and_utf8() {
    let input = "α,,β,\n";
    let reader = CsvRecords::new(Cursor::new(input.as_bytes()), 64, 4).unwrap();
    let rows = PullStream::new(reader).try_collect_bounded(1).unwrap();
    assert_eq!(rows, vec![vec!["α", "", "β", ""]]);
}

#[test]
fn csv_rejects_oversize_multiline_record_before_emitting_partials() {
    let mut reader = CsvRecords::new(Cursor::new(b"\"123\n456\"\nsecond\n"), 8, 2).unwrap();
    assert_eq!(reader.next().unwrap().unwrap_err().kind(), ErrorKind::InvalidData);
    assert!(reader.next().is_none());
}

#[test]
fn csv_rejects_more_columns_than_the_configured_limit() {
    let mut reader = CsvRecords::new(Cursor::new(b"a,b,c\n"), 40, 2).unwrap();
    assert_eq!(reader.next().unwrap().unwrap_err().kind(), ErrorKind::InvalidData);
    assert!(reader.next().is_none());
}

#[test]
fn csv_rejects_bare_quote_unclosed_quote_and_non_newline_cr() {
    for bytes in [&b"a\"b\n"[..], &b"\"unclosed"[..], &b"a\rb"[..]] {
        let mut reader = CsvRecords::new(Cursor::new(bytes), 64, 2).unwrap();
        assert_eq!(reader.next().unwrap().unwrap_err().kind(), ErrorKind::InvalidData);
        assert!(reader.next().is_none());
    }
}

#[test]
fn csv_rejects_zero_or_oversize_configuration_values() {
    assert!(matches!(
        CsvRecords::new(Cursor::new(b""), 0, 1),
        Err(StreamFailure::InvalidLimit)
    ));
    assert!(matches!(
        CsvRecords::new(Cursor::new(b""), 100, 0),
        Err(StreamFailure::InvalidLimit)
    ));
    assert!(matches!(
        JsonlRecords::new(Cursor::new(b""), 0),
        Err(StreamFailure::InvalidLimit)
    ));
}

#[test]
fn csv_exact_record_budget_including_crlf_is_accepted() {
    let mut reader = CsvRecords::new(Cursor::new(b"a,b\r\n"), 5, 2).unwrap();
    assert_eq!(reader.next().unwrap().unwrap(), vec!["a", "b"]);
    assert!(reader.next().is_none());
}

#[test]
fn csv_empty_input_has_no_rows_but_explicit_empty_row_does() {
    let mut empty = CsvRecords::new(Cursor::new(b""), 16, 2).unwrap();
    assert!(empty.next().is_none());
    let mut blank = CsvRecords::new(Cursor::new(b"\n"), 16, 2).unwrap();
    assert_eq!(blank.next().unwrap().unwrap(), vec![""]);
    assert!(blank.next().is_none());
}
