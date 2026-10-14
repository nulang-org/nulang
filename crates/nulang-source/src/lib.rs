//! Dependency-light source locations and source-map support for Nulang.
//!
//! This crate intentionally depends only on the Rust standard library so the
//! lexer and other frontend crates can compile without pulling the root
//! compiler crate into their dependency graph.

use std::cell::RefCell;

thread_local! {
    /// Source map for the current compilation unit.
    static SOURCE_MAP: RefCell<Option<SourceMap>> = const { RefCell::new(None) };
}

/// Maps byte offsets to 1-indexed line/column positions.
#[derive(Debug, Clone)]
pub struct SourceMap {
    line_starts: Vec<u32>,
    source: String,
    file_path: Option<String>,
}

impl SourceMap {
    /// Build a source map from source text.
    pub fn new(source: &str) -> Self {
        Self::with_file(source, None)
    }

    /// Build a source map with an optional file path for diagnostics.
    pub fn with_file(source: &str, file_path: Option<&str>) -> Self {
        let mut line_starts = vec![0u32];
        for (i, &b) in source.as_bytes().iter().enumerate() {
            if b == b'\n' {
                line_starts.push(i as u32 + 1);
            }
        }
        Self {
            line_starts,
            source: source.to_string(),
            file_path: file_path.map(str::to_string),
        }
    }

    /// Resolve a byte offset to a 1-indexed line and column.
    pub fn line_col(&self, offset: u32) -> (usize, usize) {
        let idx = match self.line_starts.binary_search(&offset) {
            Ok(i) => i,
            Err(i) => i.saturating_sub(1),
        };
        let line = idx + 1;
        let col = offset.saturating_sub(self.line_starts[idx]) + 1;
        (line, col as usize)
    }

    /// Return a 1-indexed source line without its trailing newline.
    pub fn source_line(&self, line: usize) -> Option<&str> {
        if line == 0 || line > self.line_starts.len() {
            return None;
        }
        let start = self.line_starts[line - 1] as usize;
        let end = if line < self.line_starts.len() {
            (self.line_starts[line] as usize).saturating_sub(1)
        } else {
            self.source.len()
        };
        if start <= end && start <= self.source.len() {
            Some(&self.source[start..end.min(self.source.len())])
        } else {
            None
        }
    }

    /// Return a source slice starting at `offset` for at most `len` bytes.
    pub fn source_slice(&self, offset: u32, len: u32) -> Option<&str> {
        let start = offset as usize;
        let end = (start + len as usize).min(self.source.len());
        if start < self.source.len() {
            Some(&self.source[start..end])
        } else {
            None
        }
    }

    pub fn file_path(&self) -> Option<&str> {
        self.file_path.as_deref()
    }

    pub fn source_text(&self) -> &str {
        &self.source
    }
}

/// Compact source span represented by byte offsets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct Span {
    pub start: u32,
    pub end: u32,
}

impl Span {
    pub const fn new(start: u32, end: u32) -> Self {
        Self { start, end }
    }

    pub fn line(&self) -> usize {
        with_source_map(|sm| sm.map(|sm| sm.line_col(self.start).0).unwrap_or(0))
    }

    pub fn column(&self) -> usize {
        with_source_map(|sm| sm.map(|sm| sm.line_col(self.start).1).unwrap_or(0))
    }

    pub fn end_line(&self) -> usize {
        with_source_map(|sm| sm.map(|sm| sm.line_col(self.end).0).unwrap_or(0))
    }

    pub fn end_column(&self) -> usize {
        with_source_map(|sm| sm.map(|sm| sm.line_col(self.end).1).unwrap_or(0))
    }

    pub fn source_line(&self) -> Option<String> {
        with_source_map(|sm| {
            sm.and_then(|sm| {
                let (line, _) = sm.line_col(self.start);
                sm.source_line(line).map(str::to_string)
            })
        })
    }

    pub fn file(&self) -> Option<String> {
        with_source_map(|sm| sm.and_then(|sm| sm.file_path().map(str::to_string)))
    }
}

fn with_source_map<T>(f: impl FnOnce(Option<&SourceMap>) -> T) -> T {
    SOURCE_MAP.with(|slot| {
        let map = slot.borrow();
        f(map.as_ref())
    })
}

pub fn set_source_map(source: &str) {
    set_source_map_with_file(source, None);
}

pub fn set_source_map_with_file(source: &str, file: Option<&str>) {
    SOURCE_MAP.with(|slot| {
        *slot.borrow_mut() = Some(SourceMap::with_file(source, file));
    });
}

pub fn clear_source_map() {
    SOURCE_MAP.with(|slot| *slot.borrow_mut() = None);
}

pub fn source_map_file() -> Option<String> {
    with_source_map(|sm| sm.and_then(|sm| sm.file_path().map(str::to_string)))
}

pub fn current_source_text() -> Option<String> {
    with_source_map(|sm| sm.map(|sm| sm.source_text().to_string()))
}

pub fn source_slice_for_span(span: Span) -> Option<String> {
    with_source_map(|sm| {
        sm.and_then(|sm| {
            sm.source_slice(span.start, span.end.saturating_sub(span.start))
                .map(str::to_string)
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_lines_columns_and_slices() {
        clear_source_map();
        set_source_map_with_file("one\ntwo\nthree", Some("main.nula"));
        let span = Span::new(4, 7);
        assert_eq!((span.line(), span.column()), (2, 1));
        assert_eq!((span.end_line(), span.end_column()), (2, 4));
        assert_eq!(span.source_line().as_deref(), Some("two"));
        assert_eq!(span.file().as_deref(), Some("main.nula"));
        assert_eq!(source_slice_for_span(span).as_deref(), Some("two"));
    }

    #[test]
    fn unset_map_uses_zero_positions() {
        clear_source_map();
        let span = Span::new(3, 4);
        assert_eq!((span.line(), span.column()), (0, 0));
        assert_eq!(span.source_line(), None);
    }
}
