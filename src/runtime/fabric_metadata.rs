//! Chunked, generation-atomic metadata transfer for Nulang Fabric.
//!
//! Fabric metadata snapshots (subscriptions, service advertisements, load
//! summaries) must never be partially installed. This module splits an opaque
//! encoded snapshot into bounded chunks and reassembles those chunks without
//! exposing any payload until every chunk for one generation is present and the
//! complete BLAKE3 digest verifies.
//!
//! The payload is deliberately opaque here. Subscription/service codecs and
//! NUL0 framing live at their existing boundaries; this primitive only owns the
//! transport-independent convergence invariant.

use std::collections::{BTreeMap, HashMap, HashSet};

/// Logical metadata namespace carried by Fabric convergence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FabricMetadataKind {
    Subscriptions,
    Services,
    Load,
}

/// One bounded piece of a complete Fabric metadata generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FabricMetadataChunk {
    /// Stable owner identity (normally `NodeId.0`).
    pub owner: u64,
    pub kind: FabricMetadataKind,
    /// Monotonically increasing generation for this owner + metadata kind.
    pub generation: u64,
    /// Digest of the complete encoded snapshot, identical on every chunk.
    pub snapshot_hash: [u8; 32],
    /// Zero-based chunk index.
    pub chunk_index: u32,
    /// Total number of chunks in this snapshot.
    pub chunk_count: u32,
    /// Opaque encoded snapshot bytes for this chunk.
    pub payload: Vec<u8>,
}

/// A fully reassembled and verified snapshot safe for atomic installation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FabricMetadataSnapshot {
    pub owner: u64,
    pub kind: FabricMetadataKind,
    pub generation: u64,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetadataAssemblyError {
    InvalidChunk,
    StaleGeneration,
    ConflictingSnapshot,
    ConflictingChunk,
    TooManyChunks,
    TooManyInflightSnapshots,
    SnapshotTooLarge,
    InflightBytesExceeded,
    SnapshotHashMismatch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetadataScheduleError {
    EmptySnapshot,
    IncompleteSnapshot,
    MixedSnapshot,
    StaleGeneration,
    ConflictingGeneration,
}

/// Split one complete encoded snapshot into bounded chunks.
///
/// Even an empty snapshot emits exactly one chunk so an authoritative empty
/// generation can be distinguished from an omitted metadata update.
pub fn chunk_snapshot(
    kind: FabricMetadataKind,
    owner: u64,
    generation: u64,
    payload: &[u8],
    max_chunk_bytes: usize,
) -> Result<Vec<FabricMetadataChunk>, MetadataAssemblyError> {
    if generation == 0 || max_chunk_bytes == 0 {
        return Err(MetadataAssemblyError::InvalidChunk);
    }

    let digest = *blake3::hash(payload).as_bytes();
    let chunk_count = if payload.is_empty() {
        1usize
    } else {
        payload.len().div_ceil(max_chunk_bytes)
    };
    if chunk_count > u32::MAX as usize {
        return Err(MetadataAssemblyError::TooManyChunks);
    }

    let mut chunks = Vec::with_capacity(chunk_count);
    if payload.is_empty() {
        chunks.push(FabricMetadataChunk {
            owner,
            kind,
            generation,
            snapshot_hash: digest,
            chunk_index: 0,
            chunk_count: 1,
            payload: Vec::new(),
        });
        return Ok(chunks);
    }

    for (index, bytes) in payload.chunks(max_chunk_bytes).enumerate() {
        chunks.push(FabricMetadataChunk {
            owner,
            kind,
            generation,
            snapshot_hash: digest,
            chunk_index: index as u32,
            chunk_count: chunk_count as u32,
            payload: bytes.to_vec(),
        });
    }
    Ok(chunks)
}

#[derive(Debug, Clone, Copy)]
struct MetadataCursorState {
    generation: u64,
    snapshot_hash: [u8; 32],
    chunk_count: u32,
    next_index: u32,
}

/// Deterministic sender-side rotation over a complete metadata snapshot.
///
/// Gossip is lossy by design. Repeated calls therefore cycle through every
/// chunk forever instead of treating one pass as delivery confirmation. A
/// newer generation resets immediately to chunk zero, while conflicting bytes
/// under the same generation fail closed.
#[derive(Debug, Default)]
pub struct FabricMetadataChunkCursor {
    cursors: HashMap<(FabricMetadataKind, u64), MetadataCursorState>,
}

impl FabricMetadataChunkCursor {
    pub fn next<'a>(
        &mut self,
        chunks: &'a [FabricMetadataChunk],
    ) -> Result<&'a FabricMetadataChunk, MetadataScheduleError> {
        let first = chunks.first().ok_or(MetadataScheduleError::EmptySnapshot)?;
        if first.generation == 0 || first.chunk_count == 0 {
            return Err(MetadataScheduleError::MixedSnapshot);
        }
        if chunks.len() != first.chunk_count as usize {
            return Err(MetadataScheduleError::IncompleteSnapshot);
        }

        let mut seen = HashSet::with_capacity(chunks.len());
        for chunk in chunks {
            if chunk.owner != first.owner
                || chunk.kind != first.kind
                || chunk.generation != first.generation
                || chunk.snapshot_hash != first.snapshot_hash
                || chunk.chunk_count != first.chunk_count
                || chunk.chunk_index >= chunk.chunk_count
            {
                return Err(MetadataScheduleError::MixedSnapshot);
            }
            if !seen.insert(chunk.chunk_index) {
                return Err(MetadataScheduleError::MixedSnapshot);
            }
        }
        if seen.len() != first.chunk_count as usize {
            return Err(MetadataScheduleError::IncompleteSnapshot);
        }

        let key = (first.kind, first.owner);
        let target_index = match self.cursors.get(&key).copied() {
            Some(current) if first.generation < current.generation => {
                return Err(MetadataScheduleError::StaleGeneration);
            }
            Some(current) if first.generation == current.generation => {
                if first.snapshot_hash != current.snapshot_hash
                    || first.chunk_count != current.chunk_count
                {
                    return Err(MetadataScheduleError::ConflictingGeneration);
                }
                current.next_index
            }
            Some(_) | None => 0,
        };

        let selected = chunks
            .iter()
            .find(|chunk| chunk.chunk_index == target_index)
            .ok_or(MetadataScheduleError::IncompleteSnapshot)?;
        let next_index = (target_index + 1) % first.chunk_count;
        self.cursors.insert(
            key,
            MetadataCursorState {
                generation: first.generation,
                snapshot_hash: first.snapshot_hash,
                chunk_count: first.chunk_count,
                next_index,
            },
        );
        Ok(selected)
    }

    pub fn discard_owner(&mut self, owner: u64) {
        self.cursors
            .retain(|(_, cursor_owner), _| *cursor_owner != owner);
    }
}

#[derive(Debug)]
struct InflightSnapshot {
    generation: u64,
    snapshot_hash: [u8; 32],
    chunk_count: u32,
    bytes: usize,
    last_activity_tick: u64,
    chunks: BTreeMap<u32, Vec<u8>>,
}

impl InflightSnapshot {
    fn from_chunk(chunk: &FabricMetadataChunk, activity_tick: u64) -> Self {
        Self {
            generation: chunk.generation,
            snapshot_hash: chunk.snapshot_hash,
            chunk_count: chunk.chunk_count,
            bytes: 0,
            last_activity_tick: activity_tick,
            chunks: BTreeMap::new(),
        }
    }
}

/// Reassembles bounded metadata chunks and publishes only complete generations.
///
/// At most one incomplete generation is retained for each `(kind, owner)`.
/// Receiving a newer generation atomically abandons the older incomplete one;
/// receiving an older/equal completed generation is rejected as stale.
/// Global count/byte budgets bound abandoned or adversarial incomplete state.
#[derive(Debug)]
pub struct FabricMetadataAssembler {
    max_snapshot_bytes: usize,
    max_chunks: u32,
    max_inflight_snapshots: usize,
    max_inflight_bytes: usize,
    inflight_bytes: usize,
    inflight: HashMap<(FabricMetadataKind, u64), InflightSnapshot>,
    latest_generation: HashMap<(FabricMetadataKind, u64), u64>,
}

impl FabricMetadataAssembler {
    /// Convenience constructor with conservative process-local aggregate bounds.
    /// Runtime integration should prefer [`Self::with_limits`] so the limits are
    /// explicit configuration rather than implicit policy.
    pub fn new(max_snapshot_bytes: usize, max_chunks: u32) -> Self {
        let max_inflight_bytes = max_snapshot_bytes.saturating_mul(8);
        Self::with_limits(max_snapshot_bytes, max_chunks, 64, max_inflight_bytes)
    }

    pub fn with_limits(
        max_snapshot_bytes: usize,
        max_chunks: u32,
        max_inflight_snapshots: usize,
        max_inflight_bytes: usize,
    ) -> Self {
        assert!(
            max_snapshot_bytes > 0,
            "Fabric metadata snapshot bound must be non-zero"
        );
        assert!(
            max_chunks > 0,
            "Fabric metadata chunk bound must be non-zero"
        );
        assert!(
            max_inflight_snapshots > 0,
            "Fabric metadata in-flight snapshot bound must be non-zero"
        );
        assert!(
            max_inflight_bytes > 0,
            "Fabric metadata in-flight byte bound must be non-zero"
        );
        Self {
            max_snapshot_bytes,
            max_chunks,
            max_inflight_snapshots,
            max_inflight_bytes,
            inflight_bytes: 0,
            inflight: HashMap::new(),
            latest_generation: HashMap::new(),
        }
    }

    pub fn latest_generation(&self, kind: FabricMetadataKind, owner: u64) -> Option<u64> {
        self.latest_generation.get(&(kind, owner)).copied()
    }

    pub fn inflight_snapshot_count(&self) -> usize {
        self.inflight.len()
    }

    pub fn inflight_bytes(&self) -> usize {
        self.inflight_bytes
    }

    /// Drop all incomplete and completed-generation tracking for one owner.
    ///
    /// This is intended for the same confirmed-node-removal boundary that
    /// clears Fabric's remote routing generation, allowing a genuine restart
    /// with the same stable node id to begin a fresh generation sequence.
    pub fn discard_owner(&mut self, owner: u64) {
        let keys: Vec<_> = self
            .inflight
            .keys()
            .filter(|(_, key_owner)| *key_owner == owner)
            .copied()
            .collect();
        for key in keys {
            self.remove_inflight(key);
        }
        self.latest_generation
            .retain(|(_, key_owner), _| *key_owner != owner);
    }

    /// Deterministically drop incomplete snapshots whose last accepted chunk
    /// activity is older than `cutoff_tick`.
    ///
    /// The caller owns the time domain. Runtime integration can pass its
    /// virtual/logical clock, keeping DST behavior deterministic and avoiding
    /// wall-clock reads inside this primitive.
    pub fn prune_inflight_before(&mut self, cutoff_tick: u64) -> usize {
        let keys: Vec<_> = self
            .inflight
            .iter()
            .filter_map(|(key, state)| {
                (state.last_activity_tick < cutoff_tick).then_some(*key)
            })
            .collect();
        let removed = keys.len();
        for key in keys {
            self.remove_inflight(key);
        }
        removed
    }

    /// Add one chunk without caller-supplied logical time.
    ///
    /// This is suitable when expiration is not used. Runtime paths that prune
    /// abandoned assemblies should call [`Self::push_at`] instead.
    pub fn push(
        &mut self,
        chunk: FabricMetadataChunk,
    ) -> Result<Option<FabricMetadataSnapshot>, MetadataAssemblyError> {
        self.push_at(chunk, 0)
    }

    /// Add one chunk at the caller's deterministic logical time. Returns a
    /// verified snapshot only when the generation is complete; partial
    /// generations are never surfaced.
    pub fn push_at(
        &mut self,
        chunk: FabricMetadataChunk,
        activity_tick: u64,
    ) -> Result<Option<FabricMetadataSnapshot>, MetadataAssemblyError> {
        if chunk.generation == 0
            || chunk.chunk_count == 0
            || chunk.chunk_count > self.max_chunks
            || chunk.chunk_index >= chunk.chunk_count
        {
            return Err(if chunk.chunk_count > self.max_chunks {
                MetadataAssemblyError::TooManyChunks
            } else {
                MetadataAssemblyError::InvalidChunk
            });
        }
        if chunk.payload.len() > self.max_snapshot_bytes {
            return Err(MetadataAssemblyError::SnapshotTooLarge);
        }

        let key = (chunk.kind, chunk.owner);
        if self
            .latest_generation
            .get(&key)
            .is_some_and(|latest| chunk.generation <= *latest)
        {
            return Err(MetadataAssemblyError::StaleGeneration);
        }

        if let Some(current) = self.inflight.get(&key) {
            if chunk.generation < current.generation {
                return Err(MetadataAssemblyError::StaleGeneration);
            }
            if chunk.generation == current.generation
                && (current.snapshot_hash != chunk.snapshot_hash
                    || current.chunk_count != chunk.chunk_count)
            {
                self.remove_inflight(key);
                return Err(MetadataAssemblyError::ConflictingSnapshot);
            }
            if chunk.generation > current.generation {
                self.remove_inflight(key);
            }
        }

        if !self.inflight.contains_key(&key) {
            if self.inflight.len() >= self.max_inflight_snapshots {
                return Err(MetadataAssemblyError::TooManyInflightSnapshots);
            }
            self.inflight
                .insert(key, InflightSnapshot::from_chunk(&chunk, activity_tick));
        }

        if let Some(existing) = self
            .inflight
            .get(&key)
            .and_then(|state| state.chunks.get(&chunk.chunk_index))
        {
            if existing == &chunk.payload {
                if let Some(state) = self.inflight.get_mut(&key) {
                    state.last_activity_tick = activity_tick;
                }
                return Ok(None);
            }
            self.remove_inflight(key);
            return Err(MetadataAssemblyError::ConflictingChunk);
        }

        let state_bytes = self
            .inflight
            .get(&key)
            .expect("Fabric metadata state must exist before chunk insertion")
            .bytes;
        let next_state_bytes = state_bytes
            .checked_add(chunk.payload.len())
            .ok_or(MetadataAssemblyError::SnapshotTooLarge)?;
        if next_state_bytes > self.max_snapshot_bytes {
            self.remove_inflight(key);
            return Err(MetadataAssemblyError::SnapshotTooLarge);
        }

        let next_inflight_bytes = self
            .inflight_bytes
            .checked_add(chunk.payload.len())
            .ok_or(MetadataAssemblyError::InflightBytesExceeded)?;
        if next_inflight_bytes > self.max_inflight_bytes {
            // If this generation had no accepted chunks yet, don't retain an
            // empty shell that would consume the in-flight snapshot budget.
            if state_bytes == 0 {
                self.remove_inflight(key);
            }
            return Err(MetadataAssemblyError::InflightBytesExceeded);
        }

        let complete = {
            let state = self
                .inflight
                .get_mut(&key)
                .expect("Fabric metadata state must exist before chunk insertion");
            state.bytes = next_state_bytes;
            state.last_activity_tick = activity_tick;
            state.chunks.insert(chunk.chunk_index, chunk.payload);
            state.chunks.len() == state.chunk_count as usize
        };
        self.inflight_bytes = next_inflight_bytes;

        if !complete {
            return Ok(None);
        }

        let state = self
            .inflight
            .remove(&key)
            .expect("complete Fabric metadata state must exist");
        self.inflight_bytes = self.inflight_bytes.saturating_sub(state.bytes);

        let mut payload = Vec::with_capacity(state.bytes);
        for index in 0..state.chunk_count {
            let Some(bytes) = state.chunks.get(&index) else {
                return Err(MetadataAssemblyError::InvalidChunk);
            };
            payload.extend_from_slice(bytes);
        }

        if blake3::hash(&payload).as_bytes() != &state.snapshot_hash {
            return Err(MetadataAssemblyError::SnapshotHashMismatch);
        }

        self.latest_generation.insert(key, state.generation);
        Ok(Some(FabricMetadataSnapshot {
            owner: chunk.owner,
            kind: chunk.kind,
            generation: state.generation,
            payload,
        }))
    }

    fn remove_inflight(&mut self, key: (FabricMetadataKind, u64)) {
        if let Some(state) = self.inflight.remove(&key) {
            self.inflight_bytes = self.inflight_bytes.saturating_sub(state.bytes);
        }
    }
}
