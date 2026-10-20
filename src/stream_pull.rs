//! Experimental, bounded-memory pull-stream primitives for trusted Rust hosts.
//!
//! No language syntax, bytecode, thread spawning, actor dispatch, or durability
//! is added here. Pull-based demand is synchronous: producers are polled only
//! when consumers request the next item. This is not an async channel.

use std::fmt;
use std::io::{self, BufRead};

/// Errors from a bounded terminal consumer operation.
#[derive(Debug)]
pub enum StreamFailure<E> {
    Source(E),
    InvalidLimit,
    LimitExceeded { limit: usize },
}

impl<E: fmt::Display> fmt::Display for StreamFailure<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Source(error) => write!(f, "stream source failed: {error}"),
            Self::InvalidLimit => write!(f, "stream item limit must be positive"),
            Self::LimitExceeded { limit } => write!(f, "stream exceeds {limit} items"),
        }
    }
}

impl<E: std::error::Error + 'static> std::error::Error for StreamFailure<E> {}

/// Zero-prefetch iterator adapter with explicit fallible transforms.
///
/// Transformations preserve input errors. No elements are pulled until a
/// terminal consumer (or a subsequent stage) requests them.
pub struct PullStream<I> {
    iter: I,
}

impl<I> PullStream<I>
where
    I: Iterator,
{
    pub fn new(iter: I) -> Self {
        Self { iter }
    }

    /// Restrict demand to at most `limit` items; never prefetch item N + 1.
    pub fn take(self, limit: usize) -> PullStream<std::iter::Take<I>> {
        PullStream::new(self.iter.take(limit))
    }
}

impl<I, T, E> PullStream<I>
where
    I: Iterator<Item = Result<T, E>>,
{
    /// Apply `f` to successful items; propagate source errors untouched.
    pub fn map_ok<U, F>(self, mut f: F) -> PullStream<impl Iterator<Item = Result<U, E>>>
    where
        F: FnMut(T) -> U,
    {
        PullStream::new(self.iter.map(move |result| result.map(&mut f)))
    }

    /// Select successful items; source errors always pass to the consumer.
    pub fn filter_ok<F>(self, mut predicate: F) -> PullStream<impl Iterator<Item = Result<T, E>>>
    where
        F: FnMut(&T) -> bool,
    {
        PullStream::new(self.iter.filter(move |item| match item {
            Ok(value) => predicate(value),
            Err(_) => true,
        }))
    }

    /// Fold success values in source order; stop immediately at first error.
    pub fn try_fold_ok<A, F>(self, mut initial: A, mut f: F) -> Result<A, E>
    where
        F: FnMut(A, T) -> A,
    {
        for item in self.iter {
            initial = f(initial, item?);
        }
        Ok(initial)
    }

    /// Collect with an explicit hard item count cap (no silent truncation).
    /// Detecting an overflow necessarily pulls the first excessive item.
    pub fn try_collect_bounded(self, limit: usize) -> Result<Vec<T>, StreamFailure<E>> {
        if limit == 0 {
            return Err(StreamFailure::InvalidLimit);
        }

        let mut out = Vec::new();
        for item in self.iter {
            let item = item.map_err(StreamFailure::Source)?;
            if out.len() >= limit {
                return Err(StreamFailure::LimitExceeded { limit });
            }
            out.push(item);
        }
        Ok(out)
    }
}

/// Upper bound on the UTF-8 line bytes, including the line ending.
/// All valid `BoundedLines` instances have a nonzero, finite record cap.
pub const MAX_LINE_BYTES: usize = 16 * 1024 * 1024;

/// An incremental UTF-8 line reader that fails closed on oversized records.
///
/// Unlike `BufRead::lines`, this reader checks the cap *before* accumulating
/// each chunk. A malformed UTF-8 record or I/O error terminates the iterator.
/// A successful line strips LF, then strips one CR only in the CRLF case.
/// The count cap includes the original delimiter bytes.
pub struct BoundedLines<R> {
    reader: R,
    max_line_bytes: usize,
    finished: bool,
}

impl<R: BufRead> BoundedLines<R> {
    pub fn new(reader: R, max_line_bytes: usize) -> Result<Self, StreamFailure<io::Error>> {
        if max_line_bytes == 0 || max_line_bytes > MAX_LINE_BYTES {
            return Err(StreamFailure::InvalidLimit);
        }

        Ok(Self {
            reader,
            max_line_bytes,
            finished: false,
        })
    }
}

impl<R: BufRead> Iterator for BoundedLines<R> {
    type Item = io::Result<String>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.finished {
            return None;
        }

        let mut bytes = Vec::new();
        let mut saw_newline = false;

        loop {
            let available = match self.reader.fill_buf() {
                Ok(bytes) => bytes,
                Err(error) => {
                    self.finished = true;
                    return Some(Err(error));
                }
            };

            if available.is_empty() {
                self.finished = true;
                if bytes.is_empty() {
                    return None;
                }
                break;
            }

            let newline = available.iter().position(|byte| *byte == b'\n');
            let consumed = newline.map_or(available.len(), |index| index + 1);
            if bytes
                .len()
                .checked_add(consumed)
                .is_none_or(|total| total > self.max_line_bytes)
            {
                self.finished = true;
                return Some(Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "input line exceeds configured byte limit",
                )));
            }

            bytes.extend_from_slice(&available[..consumed]);
            self.reader.consume(consumed);
            if newline.is_some() {
                saw_newline = true;
                break;
            }
        }

        if saw_newline {
            bytes.pop(); // LF
            if bytes.last() == Some(&b'\r') {
                bytes.pop(); // CR in CRLF only
            }
        }

        match String::from_utf8(bytes) {
            Ok(line) => Some(Ok(line)),
            Err(error) => {
                self.finished = true;
                Some(Err(io::Error::new(io::ErrorKind::InvalidData, error)))
            }
        }
    }
}
