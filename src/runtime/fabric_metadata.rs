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

use std::collections::{BTreeMap, HashMap};

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
    SnapshotTooLarge,
    SnapshotHashMismatch,
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

#[derive(Debug)]
struct InflightSnapshot {
    generation: u64,
    snapshot_hash: [u8; 32],
    chunk_count: u32,
    bytes: usize,
    chunks: BTreeMap<u32, Vec<u8>>,
}

impl InflightSnapshot {
    fn from_chunk(chunk: &FabricMetadataChunk) -> Self {
        Self {
            generation: chunk.generation,
            snapshot_hash: chunk.snapshot_hash,
            chunk_count: chunk.chunk_count,
            bytes: 0,
            chunks: BTreeMap::new(),
        }
    }
}

/// Reassembles bounded metadata chunks and publishes only complete generations.
///
/// At most one incomplete generation is retained for each `(kind, owner)`.
/// Receiving a newer generation atomically abandons the older incomplete one;
/// receiving an older/equal completed generation is rejected as stale.
#[derive(Debug)]
pub struct FabricMetadataAssembler {
    max_snapshot_bytes: usize,
    max_chunks: u32,
    inflight: HashMap<(FabricMetadataKind, u64), InflightSnapshot>,
    latest_generation: HashMap<(FabricMetadataKind, u64), u64>,
}

impl FabricMetadataAssembler {
    pub fn new(max_snapshot_bytes: usize, max_chunks: u32) -> Self {
        assert!(max_snapshot_bytes > 0, "Fabric metadata snapshot bound must be non-zero");
        assert!(max_chunks > 0, "Fabric metadata chunk bound must be non-zero");
        Self {
            max_snapshot_bytes,
            max_chunks,
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

    /// Drop all incomplete and completed-generation tracking for one owner.
    ///
    /// This is intended for the same confirmed-node-removal boundary that
    /// clears Fabric's remote routing generation, allowing a genuine restart
    /// with the same stable node id to begin a fresh generation sequence.
    pub fn discard_owner(&mut self, owner: u64) {
        self.inflight.retain(|(_, key_owner), _| *key_owner != owner);
        self.latest_generation
            .retain(|(_, key_owner), _| *key_owner != owner);
    }

    /// Add one chunk. Returns a verified snapshot only when the generation is
    /// complete; partial generations are never surfaced.
    pub fn push(
        &mut self,
        chunk: FabricMetadataChunk,
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

        match self.inflight.get(&key) {
            Some(current) if chunk.generation < current.generation => {
                return Err(MetadataAssemblyError::StaleGeneration);
            }
            Some(current) if chunk.generation == current.generation => {
                if current.snapshot_hash != chunk.snapshot_hash
                    || current.chunk_count != chunk.chunk_count
                {
                    self.inflight.remove(&key);
                    return Err(MetadataAssemblyError::ConflictingSnapshot);
                }
            }
            Some(_) => {
                // A newer generation supersedes the incomplete old generation.
                self.inflight.remove(&key);
            }
            None => {}
        }

        let state = self
            .inflight
            .entry(key)
            .or_insert_with(|| InflightSnapshot::from_chunk(&chunk));

        if let Some(existing) = state.chunks.get(&chunk.chunk_index) {
            if existing == &chunk.payload {
                return Ok(None);
            }
            self.inflight.remove(&key);
            return Err(MetadataAssemblyError::ConflictingChunk);
        }

        let next_bytes = state
            .bytes
            .checked_add(chunk.payload.len())
            .ok_or(MetadataAssemblyError::SnapshotTooLarge)?;
        if next_bytes > self.max_snapshot_bytes {
            self.inflight.remove(&key);
            return Err(MetadataAssemblyError::SnapshotTooLarge);
        }
        state.bytes = next_bytes;
        state.chunks.insert(chunk.chunk_index, chunk.payload);

        if state.chunks.len() != state.chunk_count as usize {
            return Ok(None);
        }

        let mut payload = Vec::with_capacity(state.bytes);
        for index in 0..state.chunk_count {
            let Some(bytes) = state.chunks.get(&index) else {
                return Ok(None);
            };
            payload.extend_from_slice(bytes);
        }

        let expected_hash = state.snapshot_hash;
        let generation = state.generation;
        self.inflight.remove(&key);
        if blake3::hash(&payload).as_bytes() != &expected_hash {
            return Err(MetadataAssemblyError::SnapshotHashMismatch);
        }

        self.latest_generation.insert(key, generation);
        Ok(Some(FabricMetadataSnapshot {
            owner: chunk.owner,
            kind: chunk.kind,
            generation,
            payload,
        }))
    }
}
