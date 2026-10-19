//! Experimental owned numerical storage and read-only contiguous views.
//!
//! This Rust package is **not** the Nulang language's existing `[T]` array
//! representation. It does not change effect dispatch, actor transfer,
//! persistence, FFI or bytecode/wire formats.

use std::ops::Range;
use std::sync::Arc;

mod private {
    pub trait Sealed {}
    impl Sealed for i64 {}
    impl Sealed for f64 {}
}

/// Element types admitted by this first experimental storage prototype.
pub trait Numeric: private::Sealed + Copy + Send + Sync + 'static {}

impl Numeric for i64 {}
impl Numeric for f64 {}

/// An invalid half-open slice range in a given view/buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewError {
    InvalidRange {
        start: usize,
        end: usize,
        available: usize,
    },
    ArithmeticOverflow,
}

/// Homogeneous, immutable numerical storage.
#[derive(Debug, Clone)]
pub struct NumericBuffer<T: Numeric> {
    storage: Arc<[T]>,
}

/// An owned, read-only view of a contiguous subrange.
///
/// The view retains the original allocation using an Arc. Since no mutable
/// references to that allocation are exposed, read-only views may overlap.
#[derive(Debug, Clone)]
pub struct NumericView<T: Numeric> {
    storage: Arc<[T]>,
    offset: usize,
    length: usize,
}

fn checked_range(range: Range<usize>, available: usize) -> Result<Range<usize>, ViewError> {
    if range.start > range.end || range.end > available {
        Err(ViewError::InvalidRange {
            start: range.start,
            end: range.end,
            available,
        })
    } else {
        Ok(range)
    }
}

impl<T: Numeric> NumericBuffer<T> {
    /// Own `values` without introducing a second element-wise copy.
    pub fn new(values: Vec<T>) -> Self {
        Self {
            storage: values.into(),
        }
    }

    pub fn len(&self) -> usize {
        self.storage.len()
    }

    pub fn is_empty(&self) -> bool {
        self.storage.is_empty()
    }

    pub fn as_slice(&self) -> &[T] {
        &self.storage
    }

    pub fn get(&self, index: usize) -> Option<&T> {
        self.storage.get(index)
    }

    /// Create an owned, zero-copy read-only view of a half-open range.
    pub fn view(&self, range: Range<usize>) -> Result<NumericView<T>, ViewError> {
        let range = checked_range(range, self.len())?;
        Ok(NumericView {
            storage: Arc::clone(&self.storage),
            offset: range.start,
            length: range.end - range.start,
        })
    }
}

impl<T: Numeric> NumericView<T> {
    pub fn len(&self) -> usize {
        self.length
    }

    pub fn is_empty(&self) -> bool {
        self.length == 0
    }

    pub fn as_slice(&self) -> &[T] {
        &self.storage[self.offset..self.offset + self.length]
    }

    pub fn get(&self, index: usize) -> Option<&T> {
        self.as_slice().get(index)
    }

    /// Further restrict a view without allocating or copying elements.
    pub fn view(&self, range: Range<usize>) -> Result<Self, ViewError> {
        let range = checked_range(range, self.length)?;
        let offset = self
            .offset
            .checked_add(range.start)
            .ok_or(ViewError::ArithmeticOverflow)?;
        Ok(Self {
            storage: Arc::clone(&self.storage),
            offset,
            length: range.end - range.start,
        })
    }
}
