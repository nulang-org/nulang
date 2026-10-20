//! End-to-end contract tests for lazy, bounded pull-based data processing.
//! This host-side prototype does not introduce a Nulang-language Stream[T]
//! type or claim actor/cloud scheduling guarantees.

use nulang::stream_pull::{BoundedLines, PullStream, StreamFailure};
use std::io::{Cursor, ErrorKind};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

#[test]
fn lazy_filter_and_map_only_pull_requested_items() {
    let reads = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&reads);
    let source = (0..10).map(move |n| {
        observed.fetch_add(1, Ordering::SeqCst);
        Ok::<i32, &'static str>(n)
    });
    let stream = PullStream::new(source)
        .map_ok(|n| n * 2)
        .filter_ok(|n| n % 4 == 0)
        .take(2);

    assert_eq!(reads.load(Ordering::SeqCst), 0);
    assert_eq!(stream.try_collect_bounded(2).unwrap(), vec![0, 4]);
    assert_eq!(reads.load(Ordering::SeqCst), 3);
}

#[test]
fn bounded_collection_fails_instead_of_silently_truncating() {
    let stream = PullStream::new((0..3).map(Ok::<_, ()>));
    assert!(matches!(
        stream.try_collect_bounded(2),
        Err(StreamFailure::LimitExceeded { limit: 2 })
    ));
}

#[test]
fn rejects_zero_collection_limit_even_for_empty_input() {
    let stream = PullStream::new(std::iter::empty::<Result<i32, ()>>());
    assert!(matches!(
        stream.try_collect_bounded(0),
        Err(StreamFailure::InvalidLimit)
    ));
}

#[test]
fn source_errors_are_not_swallowed_by_filtering() {
    let source = [Ok(1), Err("failed"), Ok(3)];
    let stream = PullStream::new(source.into_iter())
        .filter_ok(|value| *value > 10);
    assert!(matches!(
        stream.try_collect_bounded(8),
        Err(StreamFailure::Source("failed"))
    ));
}

#[test]
fn bounded_lines_support_crlf_empty_lines_and_unterminated_tail() {
    let source = Cursor::new(b"first\r\n\nlast");
    let lines = BoundedLines::new(source, 8).unwrap();
    let output = PullStream::new(lines).try_collect_bounded(8).unwrap();
    assert_eq!(output, vec!["first", "", "last"]);
}

#[test]
fn bounded_lines_reject_oversize_records_and_stop() {
    let source = Cursor::new(b"ab\ncdefghijk\nsmall\n");
    let mut lines = BoundedLines::new(source, 4).unwrap();
    assert_eq!(lines.next().unwrap().unwrap(), "ab");
    let error = lines.next().unwrap().unwrap_err();
    assert_eq!(error.kind(), ErrorKind::InvalidData);
    assert!(lines.next().is_none(), "error must terminate the stream");
}

#[test]
fn exact_line_limit_including_newline_is_accepted() {
    let lines = BoundedLines::new(Cursor::new(b"abc\n"), 4).unwrap();
    let output = PullStream::new(lines).try_collect_bounded(4).unwrap();
    assert_eq!(output, ["abc"]);
}

#[test]
fn rejects_invalid_utf8_without_resuming() {
    let mut lines = BoundedLines::new(Cursor::new(vec![0xff, b'\n']), 16).unwrap();
    assert_eq!(lines.next().unwrap().unwrap_err().kind(), ErrorKind::InvalidData);
    assert!(lines.next().is_none());
}

#[test]
fn rejects_zero_or_excessive_record_limits() {
    assert!(BoundedLines::new(Cursor::new(b""), 0).is_err());
    assert!(BoundedLines::new(Cursor::new(b""), 16 * 1024 * 1024 + 1).is_err());
}

#[test]
fn bounded_lines_are_pulled_one_record_at_a_time() {
    let reads = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&reads);
    let data = (0..200).map(move |n| {
        observed.fetch_add(1, Ordering::SeqCst);
        Ok::<_, &'static str>(format!("line{n}"))
    });
    let collected = PullStream::new(data).take(2).try_collect_bounded(2).unwrap();
    assert_eq!(collected, ["line0", "line1"]);
    assert_eq!(reads.load(Ordering::SeqCst), 2);
}
