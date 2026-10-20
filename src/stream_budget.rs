//! Explicit terminal collection budgets for fallible pull-based iterators.
//!
//! The caller supplies an *encoded payload size* for each item. That value
//! is a domain-level byte budget, NOT Rust heap RSS or allocator accounting.

use std::fmt;

/// Upper bound for a single collection's declared payload bytes.
pub const MAX_COLLECTION_PAYLOAD_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_COLLECTION_ITEMS: usize = 1_000_000;

/// Both budgets must be positive and within documented hard caps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CollectionBudget {
    max_items: usize,
    max_payload_bytes: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetConfigError {
    InvalidItems,
    InvalidBytes,
}

impl fmt::Display for BudgetConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidItems => write!(f, "invalid collection item budget"),
            Self::InvalidBytes => write!(f, "invalid collection payload-byte budget"),
        }
    }
}

impl std::error::Error for BudgetConfigError {}

impl CollectionBudget {
    pub fn new(max_items: usize, max_payload_bytes: usize) -> Result<Self, BudgetConfigError> {
        if max_items == 0 || max_items > MAX_COLLECTION_ITEMS {
            return Err(BudgetConfigError::InvalidItems);
        }
        if max_payload_bytes == 0 || max_payload_bytes > MAX_COLLECTION_PAYLOAD_BYTES {
            return Err(BudgetConfigError::InvalidBytes);
        }
        Ok(Self {
            max_items,
            max_payload_bytes,
        })
    }
}

#[derive(Debug)]
pub enum BudgetError<E> {
    Source(E),
    ItemLimit { limit: usize },
    ByteLimit { limit: usize },
}

impl<E: fmt::Display> fmt::Display for BudgetError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Source(error) => write!(f, "stream source failed: {error}"),
            Self::ItemLimit { limit } => write!(f, "stream exceeds {limit} items"),
            Self::ByteLimit { limit } => write!(f, "stream exceeds {limit} payload bytes"),
        }
    }
}

impl<E: std::error::Error + 'static> std::error::Error for BudgetError<E> {}

/// Materialize only when *both* the item count and caller-measured total
/// payload bytes fit within explicit limits. Detecting a size/count excess
/// consumes the first excess item, but never pulls later items.
///
/// A failed collection drops the partial result and returns only the error.
/// The caller's measurement callback must conservatively measure every
/// relevant byte of each item. It is not a heap memory, CPU, or time limit.
pub fn collect_budgeted<I, T, E, F>(
    items: I,
    limits: CollectionBudget,
    mut payload_bytes: F,
) -> Result<Vec<T>, BudgetError<E>>
where
    I: IntoIterator<Item = Result<T, E>>,
    F: FnMut(&T) -> usize,
{
    let mut result = Vec::new();
    let mut used_bytes = 0usize;

    for item in items {
        let value = item.map_err(BudgetError::Source)?;
        if result.len() >= limits.max_items {
            return Err(BudgetError::ItemLimit {
                limit: limits.max_items,
            });
        }

        let declared_bytes = payload_bytes(&value);
        let next_bytes = used_bytes.checked_add(declared_bytes);
        match next_bytes {
            Some(next) if next <= limits.max_payload_bytes => {
                used_bytes = next;
                result.push(value);
            }
            _ => {
                return Err(BudgetError::ByteLimit {
                    limit: limits.max_payload_bytes,
                });
            }
        }
    }

    Ok(result)
}
