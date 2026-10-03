//! Active MVCC snapshot tracking for NuDB.
//!
//! The registry is intentionally process-local and policy-free. It answers one
//! safety-critical question for future compaction/GC code: what is the oldest
//! snapshot sequence that a live reader may still need?
//!
//! A `SnapshotPin` is RAII-owned. Creating a pin increments the active count for
//! its sequence; dropping or explicitly releasing it decrements that count. The
//! registry can therefore be cloned into reader/owner components without making
//! snapshot lifetime depend on one call stack.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};

#[derive(Debug, Default)]
struct SnapshotState {
    counts: BTreeMap<u64, usize>,
    active_count: usize,
}

#[derive(Debug, Clone, Default)]
pub struct SnapshotRegistry {
    inner: Arc<Mutex<SnapshotState>>,
}

impl SnapshotRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Pin one snapshot sequence until the returned guard is released or dropped.
    pub fn pin(&self, sequence: u64) -> SnapshotPin {
        let mut state = lock_state(&self.inner);
        *state.counts.entry(sequence).or_insert(0) += 1;
        state.active_count += 1;
        drop(state);

        SnapshotPin {
            inner: Arc::clone(&self.inner),
            sequence,
            active: true,
        }
    }

    /// The oldest sequence still needed by any active snapshot reader.
    pub fn oldest_live_snapshot(&self) -> Option<u64> {
        lock_state(&self.inner)
            .counts
            .first_key_value()
            .map(|(&sequence, _)| sequence)
    }

    /// Total number of active pins, including duplicate pins at one sequence.
    pub fn active_snapshot_count(&self) -> usize {
        lock_state(&self.inner).active_count
    }

    /// Conservative sequence floor that future MVCC GC may use for retention.
    ///
    /// With no active readers, the current committed sequence is the floor. With
    /// readers, the oldest live snapshot constrains the floor. A pin above the
    /// current sequence is clamped so it can never authorize deletion beyond the
    /// currently committed state.
    pub fn retention_floor(&self, current_sequence: u64) -> u64 {
        self.oldest_live_snapshot()
            .map_or(current_sequence, |oldest| oldest.min(current_sequence))
    }
}

#[derive(Debug)]
pub struct SnapshotPin {
    inner: Arc<Mutex<SnapshotState>>,
    sequence: u64,
    active: bool,
}

impl SnapshotPin {
    pub fn sequence(&self) -> u64 {
        self.sequence
    }

    /// Release this pin before the end of its lexical scope.
    pub fn release(mut self) {
        self.release_once();
    }

    fn release_once(&mut self) {
        if !self.active {
            return;
        }

        let mut state = lock_state(&self.inner);
        let should_remove = match state.counts.get_mut(&self.sequence) {
            Some(count) if *count > 1 => {
                *count -= 1;
                false
            }
            Some(_) => true,
            None => {
                debug_assert!(
                    false,
                    "snapshot pin released without a matching registry count"
                );
                self.active = false;
                return;
            }
        };
        if should_remove {
            state.counts.remove(&self.sequence);
        }
        debug_assert!(state.active_count > 0);
        state.active_count -= 1;
        self.active = false;
    }
}

impl Drop for SnapshotPin {
    fn drop(&mut self) {
        self.release_once();
    }
}

fn lock_state(inner: &Mutex<SnapshotState>) -> MutexGuard<'_, SnapshotState> {
    inner
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
