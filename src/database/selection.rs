//! Batch-local predicate selection bitmaps.
//!
//! This is an executor primitive over borrowed, snapshot-visible scan rows.
//! It is not encoded-page predicate pushdown or SIMD execution: the initial
//! implementation evaluates the caller's predicate once per visible row.
//! Bitmaps retain row positions so downstream operators can avoid copying
//! values until they need to materialize selected rows.

use std::fmt;

use super::tablet::{TabletScanBatch, TabletScanRow};

/// Positions selected inside one visible scan batch.
///
/// The bitmap borrows its source batch: callers cannot outlive it, and
/// combining equal-sized selections from different batches is rejected.
/// Bits above the visible row count are always zero.
#[derive(Debug, Clone)]
pub struct SelectionBitmap<'batch, 'row> {
    batch: &'batch TabletScanBatch<'row>,
    words: Vec<u64>,
}

// Equality compares *positions in the same batch*, never the borrowed
// row contents. Two separately allocated batches may happen to contain
// identical rows, but their row-position masks are not interchangeable.
impl<'batch, 'row> PartialEq for SelectionBitmap<'batch, 'row> {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self.batch, other.batch) && self.words == other.words
    }
}

impl Eq for SelectionBitmap<'_, '_> {}

impl<'batch, 'row> SelectionBitmap<'batch, 'row> {
    /// Evaluate a predicate over borrowed, snapshot-visible rows.
    /// Evaluation is row-wise; selection output is packed into 64-bit words.
    pub fn from_predicate<F>(batch: &'batch TabletScanBatch<'row>, mut predicate: F) -> Self
    where
        F: FnMut(TabletScanRow<'row>) -> bool,
    {
        let len = batch.rows.len();
        let mut words = vec![0_u64; len / 64 + usize::from(len % 64 != 0)];

        for (index, row) in batch.rows.iter().copied().enumerate() {
            if predicate(row) {
                words[index / 64] |= 1_u64 << (index % 64);
            }
        }

        Self { batch, words }
    }

    pub fn len(&self) -> usize {
        self.batch.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.batch.rows.is_empty()
    }

    /// Returns `None` for positions outside this batch.
    pub fn get(&self, index: usize) -> Option<bool> {
        (index < self.len()).then(|| (self.words[index / 64] & (1_u64 << (index % 64))) != 0)
    }

    pub fn count_ones(&self) -> usize {
        self.words.iter().map(|word| word.count_ones() as usize).sum()
    }

    /// Select positions that satisfy both predicates.
    pub fn and(&self, other: &Self) -> Result<Self, SelectionError> {
        self.combine(other, |left, right| left & right)
    }

    /// Select positions that satisfy either predicate.
    pub fn or(&self, other: &Self) -> Result<Self, SelectionError> {
        self.combine(other, |left, right| left | right)
    }

    fn combine(
        &self,
        other: &Self,
        op: impl Fn(u64, u64) -> u64,
    ) -> Result<Self, SelectionError> {
        if self.len() != other.len() {
            return Err(SelectionError::LengthMismatch {
                left: self.len(),
                right: other.len(),
            });
        }
        if !std::ptr::eq(self.batch, other.batch) {
            return Err(SelectionError::DifferentBatch);
        }
        Ok(Self {
            batch: self.batch,
            words: self
                .words
                .iter()
                .zip(other.words.iter())
                .map(|(&left, &right)| op(left, right))
                .collect(),
        })
    }

    /// Iterate selected row indexes in ascending order without scanning every
    /// unselected bit. Complexity is O(words + selected rows).
    pub fn selected_indices(&self) -> SelectedIndices<'_> {
        SelectedIndices {
            words: &self.words,
            next_word: 0,
            live_word: 0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SelectedIndices<'a> {
    words: &'a [u64],
    next_word: usize,
    live_word: u64,
}

impl Iterator for SelectedIndices<'_> {
    type Item = usize;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.live_word != 0 {
                let bit = self.live_word.trailing_zeros() as usize;
                self.live_word &= self.live_word - 1;
                return Some((self.next_word - 1) * 64 + bit);
            }
            self.live_word = *self.words.get(self.next_word)?;
            self.next_word += 1;
        }
    }
}

impl std::iter::FusedIterator for SelectedIndices<'_> {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionError {
    LengthMismatch { left: usize, right: usize },
    DifferentBatch,
}

impl fmt::Display for SelectionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LengthMismatch { left, right } => write!(
                f,
                "batch selection length mismatch: left={left}, right={right}"
            ),
            Self::DifferentBatch => f.write_str("cannot combine selections from different batches"),
        }
    }
}

impl std::error::Error for SelectionError {}
