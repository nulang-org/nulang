//! Sender-side cache and rotation for chunked Fabric subscription gossip.
//!
//! A subscription generation is encoded exactly once, then gossip rounds cycle
//! through its FAB1 chunks. A newer generation replaces the cached snapshot and
//! restarts at chunk zero. Runtime integration therefore pays FAS1 encoding and
//! BLAKE3 chunking cost on subscription changes, not on every gossip interval.

use std::collections::HashMap;

use super::fabric_metadata::{
    FabricMetadataChunk, FabricMetadataChunkCursor, MetadataScheduleError,
};
use super::fabric_subscription_metadata::FabricSubscriptionMetadataEntry;
use super::fabric_subscription_snapshot::{
    FabricSubscriptionSnapshotCodec, FabricSubscriptionSnapshotError,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FabricSubscriptionGossipError {
    InvalidGeneration,
    StaleGeneration,
    Snapshot(FabricSubscriptionSnapshotError),
    Schedule(MetadataScheduleError),
}

impl From<FabricSubscriptionSnapshotError> for FabricSubscriptionGossipError {
    fn from(error: FabricSubscriptionSnapshotError) -> Self {
        Self::Snapshot(error)
    }
}

impl From<MetadataScheduleError> for FabricSubscriptionGossipError {
    fn from(error: MetadataScheduleError) -> Self {
        Self::Schedule(error)
    }
}

#[derive(Debug)]
struct CachedGeneration {
    generation: u64,
    chunks: Vec<FabricMetadataChunk>,
}

#[derive(Debug)]
pub struct FabricSubscriptionGossipSender {
    codec: FabricSubscriptionSnapshotCodec,
    cursor: FabricMetadataChunkCursor,
    cached: HashMap<u64, CachedGeneration>,
    #[cfg(test)]
    encode_count: usize,
}

impl FabricSubscriptionGossipSender {
    pub fn new(codec: FabricSubscriptionSnapshotCodec) -> Self {
        Self {
            codec,
            cursor: FabricMetadataChunkCursor::default(),
            cached: HashMap::new(),
            #[cfg(test)]
            encode_count: 0,
        }
    }

    /// Return the next FAB1 chunk for `owner` + `generation`.
    ///
    /// The generation number is the cache-coherency contract: callers must
    /// increment it whenever the authoritative subscription snapshot changes.
    /// This is already how Fabric's local registry generation behaves.
    pub fn next_chunk(
        &mut self,
        owner: u64,
        generation: u64,
        subscriptions: &[FabricSubscriptionMetadataEntry],
    ) -> Result<FabricMetadataChunk, FabricSubscriptionGossipError> {
        if generation == 0 {
            return Err(FabricSubscriptionGossipError::InvalidGeneration);
        }

        match self.cached.get(&owner) {
            Some(cached) if generation < cached.generation => {
                return Err(FabricSubscriptionGossipError::StaleGeneration);
            }
            Some(cached) if generation == cached.generation => {
                return Ok(self.cursor.next(&cached.chunks)?.clone());
            }
            Some(_) | None => {}
        }

        let chunks = self.codec.encode_chunks(owner, generation, subscriptions)?;
        #[cfg(test)]
        {
            self.encode_count += 1;
        }
        self.cached.insert(
            owner,
            CachedGeneration {
                generation,
                chunks,
            },
        );
        let cached = self
            .cached
            .get(&owner)
            .expect("Fabric subscription generation was just cached");
        Ok(self.cursor.next(&cached.chunks)?.clone())
    }

    /// Drop all sender state for one owner so a confirmed restart may begin a
    /// fresh generation sequence.
    pub fn discard_owner(&mut self, owner: u64) {
        self.cached.remove(&owner);
        self.cursor.discard_owner(owner);
    }

    pub fn cached_owner_count(&self) -> usize {
        self.cached.len()
    }

    #[cfg(test)]
    pub fn encode_count(&self) -> usize {
        self.encode_count
    }
}
