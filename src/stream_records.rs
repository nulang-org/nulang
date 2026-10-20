//! Incremental JSON Lines and quoted CSV record readers for trusted Rust hosts.
//!
//! Both iterators are synchronous, pull-driven, and fail closed: malformed
//! records terminate the iterator instead of resynchronizing unpredictably.
//! A per-record byte budget bounds each raw record. This module is not a
//! Nulang-language effect or an asynchronous actor/dataflow transport.

use crate::stream_pull::{BoundedLines, StreamFailure, MAX_LINE_BYTES};
use serde_json::Value;
use std::fmt;
use std::io::{self, BufRead};

pub const MAX_CSV_FIELDS: usize = 4096;

/// Preserve the distinction between source I/O failures and JSON syntax errors.
#[derive(Debug)]
pub enum JsonlError {
    Io(io::Error),
    Parse(serde_json::Error),
}

impl fmt::Display for JsonlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "JSONL input error: {error}"),
            Self::Parse(error) => write!(f, "invalid JSONL record: {error}"),
        }
    }
}

impl std::error::Error for JsonlError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Parse(error) => Some(error),
        }
    }
}

/// JSONL records are decoded one line at a time. Empty lines are errors, not
/// silently skipped, and JSON arrays/scalars are allowed per JSON Lines.
pub struct JsonlRecords<R> {
    lines: BoundedLines<R>,
    finished: bool,
}

impl<R: BufRead> JsonlRecords<R> {
    pub fn new(reader: R, max_record_bytes: usize) -> Result<Self, StreamFailure<io::Error>> {
        Ok(Self {
            lines: BoundedLines::new(reader, max_record_bytes)?,
            finished: false,
        })
    }
}

impl<R: BufRead> Iterator for JsonlRecords<R> {
    type Item = Result<Value, JsonlError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.finished {
            return None;
        }
        match self.lines.next()? {
            Ok(line) => match serde_json::from_str(&line) {
                Ok(value) => Some(Ok(value)),
                Err(error) => {
                    self.finished = true;
                    Some(Err(JsonlError::Parse(error)))
                }
            },
            Err(error) => {
                self.finished = true;
                Some(Err(JsonlError::Io(error)))
            }
        }
    }
}

#[derive(Clone, Copy)]
enum CsvFieldState {
    Start,
    Unquoted,
    Quoted,
    AfterQuote,
}

fn invalid_csv(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

/// A byte-budgeted CSV parser. Comma-separated, double-quoted fields and
/// doubled quote escapes follow RFC 4180 conventions. LF and CRLF line
/// endings are accepted, including inside a quoted field. Bare CR outside
/// quotes and garbage after a closing quote are rejected. Records may span
/// physical lines, but cannot exceed max_record_bytes.
pub struct CsvRecords<R> {
    reader: R,
    max_record_bytes: usize,
    max_fields: usize,
    finished: bool,
}

impl<R: BufRead> CsvRecords<R> {
    pub fn new(
        reader: R,
        max_record_bytes: usize,
        max_fields: usize,
    ) -> Result<Self, StreamFailure<io::Error>> {
        if max_record_bytes == 0
            || max_record_bytes > MAX_LINE_BYTES
            || max_fields == 0
            || max_fields > MAX_CSV_FIELDS
        {
            return Err(StreamFailure::InvalidLimit);
        }
        Ok(Self {
            reader,
            max_record_bytes,
            max_fields,
            finished: false,
        })
    }

    fn next_byte(&mut self) -> io::Result<Option<u8>> {
        let available = self.reader.fill_buf()?;
        match available.first().copied() {
            Some(byte) => {
                self.reader.consume(1);
                Ok(Some(byte))
            }
            None => Ok(None),
        }
    }

    fn finish_field(&self, fields: &mut Vec<String>, field: &mut Vec<u8>) -> io::Result<()> {
        if fields.len() >= self.max_fields {
            return Err(invalid_csv("CSV record exceeds field-count limit"));
        }
        let bytes = std::mem::take(field);
        let text = String::from_utf8(bytes)
            .map_err(|_| invalid_csv("CSV field contains invalid UTF-8"))?;
        fields.push(text);
        Ok(())
    }

    fn read_record(&mut self) -> io::Result<Option<Vec<String>>> {
        let mut fields = Vec::new();
        let mut field = Vec::new();
        let mut state = CsvFieldState::Start;
        let mut read_any = false;
        let mut used = 0usize;

        loop {
            let byte = match self.next_byte()? {
                Some(byte) => byte,
                None => {
                    self.finished = true;
                    if !read_any {
                        return Ok(None);
                    }
                    if matches!(state, CsvFieldState::Quoted) {
                        return Err(invalid_csv("unclosed quoted CSV field"));
                    }
                    self.finish_field(&mut fields, &mut field)?;
                    return Ok(Some(fields));
                }
            };

            read_any = true;
            used += 1;
            if used > self.max_record_bytes {
                return Err(invalid_csv("CSV record exceeds byte limit"));
            }

            match state {
                CsvFieldState::Quoted => {
                    if byte == b'"' {
                        state = CsvFieldState::AfterQuote;
                    } else {
                        field.push(byte);
                    }
                }
                CsvFieldState::Start | CsvFieldState::Unquoted | CsvFieldState::AfterQuote => {
                    match byte {
                        b',' => {
                            self.finish_field(&mut fields, &mut field)?;
                            state = CsvFieldState::Start;
                        }
                        b'\n' => {
                            self.finish_field(&mut fields, &mut field)?;
                            return Ok(Some(fields));
                        }
                        b'\r' => {
                            if self.next_byte()? != Some(b'\n') {
                                return Err(invalid_csv("bare CR outside quoted CSV field"));
                            }
                            used += 1;
                            if used > self.max_record_bytes {
                                return Err(invalid_csv("CSV record exceeds byte limit"));
                            }
                            self.finish_field(&mut fields, &mut field)?;
                            return Ok(Some(fields));
                        }
                        b'"' if matches!(state, CsvFieldState::Start) => {
                            state = CsvFieldState::Quoted;
                        }
                        b'"' if matches!(state, CsvFieldState::AfterQuote) => {
                            field.push(b'"');
                            state = CsvFieldState::Quoted;
                        }
                        b'"' => return Err(invalid_csv("quote within unquoted CSV field")),
                        value if matches!(state, CsvFieldState::AfterQuote) => {
                            let _ = value;
                            return Err(invalid_csv("unexpected character after closing quote"));
                        }
                        value => {
                            field.push(value);
                            state = CsvFieldState::Unquoted;
                        }
                    }
                }
            }
        }
    }
}

impl<R: BufRead> Iterator for CsvRecords<R> {
    type Item = io::Result<Vec<String>>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.finished {
            return None;
        }
        match self.read_record() {
            Ok(Some(row)) => Some(Ok(row)),
            Ok(None) => None,
            Err(error) => {
                self.finished = true;
                Some(Err(error))
            }
        }
    }
}
