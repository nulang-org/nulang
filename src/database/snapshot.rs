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
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, Weak};

#[derive(Debug, Default)]
struct SnapshotState {
    counts: BTreeMap<u64, usize>,
    active_count: usize,
}

type RegistryDirectory = BTreeMap<PathBuf, Weak<Mutex<SnapshotState>>>;

#[derive(Debug, Clone, Default)]
pub struct SnapshotRegistry {
    inner: Arc<Mutex<SnapshotState>>,
}

impl SnapshotRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Reuse one process-local registry for all tablet coordinators opening the
    /// same WAL path. A live pin therefore survives closing and reopening the
    /// coordinator instead of silently disappearing from the GC floor.
    pub fn shared_for_storage_path(path: impl AsRef<Path>) -> Self {
        let key = storage_registry_key(path.as_ref());
        let directory = registry_directory();
        let mut entries = lock_directory(directory);
        entries.retain(|_, weak| weak.strong_count() != 0);

        if let Some(inner) = entries.get(&key).and_then(Weak::upgrade) {
            return Self { inner };
        }

        let inner = Arc::new(Mutex::new(SnapshotState::default()));
        entries.insert(key, Arc::downgrade(&inner));
        Self { inner }
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

    /// Pin a snapshot only when it is not newer than committed tablet state.
    pub fn pin_at_or_before(
        &self,
        requested: u64,
        committed: u64,
    ) -> Result<SnapshotPin, SnapshotError> {
        if requested > committed {
            return Err(SnapshotError::FutureSnapshot {
                requested,
                committed,
            });
        }
        Ok(self.pin(requested))
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotError {
    FutureSnapshot { requested: u64, committed: u64 },
}

impl fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FutureSnapshot {
                requested,
                committed,
            } => write!(
                f,
                "snapshot sequence {requested} is newer than committed sequence {committed}"
            ),
        }
    }
}

impl std::error::Error for SnapshotError {}

fn registry_directory() -> &'static Mutex<RegistryDirectory> {
    static DIRECTORY: OnceLock<Mutex<RegistryDirectory>> = OnceLock::new();
    DIRECTORY.get_or_init(|| Mutex::new(BTreeMap::new()))
}

fn storage_registry_key(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .map(|cwd| cwd.join(path))
                .unwrap_or_else(|_| path.to_path_buf())
        }
    })
}

fn lock_directory(directory: &Mutex<RegistryDirectory>) -> MutexGuard<'_, RegistryDirectory> {
    directory
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn lock_state(inner: &Mutex<SnapshotState>) -> MutexGuard<'_, SnapshotState> {
    inner
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
