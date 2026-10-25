//! Read-only transactional changefeed over the retained per-tablet WAL.
//!
//! Each returned `WalRecord` is one complete transaction: the WAL has synced
//! its frame before placing it in the in-memory record index, and reopening
//! revalidates that record from disk. Mutation ordering is preserved.
//!
//! This is deliberately *not* a durable subscription or distributed CDC API.
//! Checkpoint reclamation expires old cursors, and callers must surface that
//! error rather than silently skipping mutations. Consumers needing retention,
//! resumption across tablet handoff, access control, or cross-tablet ordering
//! require a separate subscriber/replication protocol.

use std::fmt;

use super::wal::{FileWal, WalRecord};

/// Cursor failures for a feed restricted to the currently retained WAL suffix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangefeedError {
    /// Records at or before this retained boundary have been reclaimed.
    HistoryPruned {
        requested_after: u64,
        oldest_available_after: u64,
    },
    /// The cursor is ahead of the tablet's last committed transaction.
    CursorAhead { requested_after: u64, latest: u64 },
}

impl fmt::Display for ChangefeedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HistoryPruned {
                requested_after,
                oldest_available_after,
            } => write!(
                f,
                "changefeed cursor {requested_after} expired; resume requires a snapshot at or after sequence {oldest_available_after}"
            ),
            Self::CursorAhead {
                requested_after,
                latest,
            } => write!(
                f,
                "changefeed cursor {requested_after} is ahead of committed sequence {latest}"
            ),
        }
    }
}

impl std::error::Error for ChangefeedError {}

/// Returns complete committed WAL transactions strictly after `after_sequence`.
///
/// The result borrows the in-memory WAL index without copying payloads. A
/// transaction's mutations are never split across records. The cursor is a
/// last-consumed sequence, not a record offset, so the same cursor can be used
/// after a clean reopen provided the needed WAL history is still retained.
///
/// A checkpoint *publication* leaves history available; WAL *reclamation*
/// advances the oldest usable cursor. There is no implicit snapshot fallback.
pub fn committed_after(
    wal: &FileWal,
    after_sequence: u64,
) -> Result<&[WalRecord], ChangefeedError> {
    let oldest_available_after = wal.base_sequence();
    if after_sequence < oldest_available_after {
        return Err(ChangefeedError::HistoryPruned {
            requested_after: after_sequence,
            oldest_available_after,
        });
    }

    let latest = wal.last_sequence();
    if after_sequence > latest {
        return Err(ChangefeedError::CursorAhead {
            requested_after: after_sequence,
            latest,
        });
    }

    // WAL recovery validates the strictly increasing sequence chain. Use a
    // monotonic partition point rather than narrowing a u64 offset to usize.
    let records = wal.records();
    let first = records.partition_point(|record| record.sequence() <= after_sequence);
    Ok(&records[first..])
}
