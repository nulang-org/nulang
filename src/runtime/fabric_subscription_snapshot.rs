//! Generation-atomic FAB1 transport bridge for Fabric subscription snapshots.
//!
//! This module composes the bounded FAS1 subscription codec with the generic
//! FAB1 chunking/assembly primitive. Callers never observe a partial
//! subscription generation: decoding happens only after all chunks are present
//! and the complete snapshot hash has verified.

use super::fabric_metadata::{
    chunk_snapshot, FabricMetadataAssembler, FabricMetadataChunk, FabricMetadataKind,
    MetadataAssemblyError,
};
use super::fabric_subscription_metadata::{
    decode_subscription_metadata, encode_subscription_metadata, FabricSubscriptionMetadataEntry,
    FabricSubscriptionMetadataError,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FabricSubscriptionSnapshot {
    pub owner: u64,
    pub generation: u64,
    pub subscriptions: Vec<FabricSubscriptionMetadataEntry>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FabricSubscriptionSnapshotError {
    WrongMetadataKind,
    Metadata(MetadataAssemblyError),
    Subscription(FabricSubscriptionMetadataError),
}

impl From<MetadataAssemblyError> for FabricSubscriptionSnapshotError {
    fn from(error: MetadataAssemblyError) -> Self {
        Self::Metadata(error)
    }
}

impl From<FabricSubscriptionMetadataError> for FabricSubscriptionSnapshotError {
    fn from(error: FabricSubscriptionMetadataError) -> Self {
        Self::Subscription(error)
    }
}

/// Sender-side policy for encoding one complete subscription generation.
#[derive(Debug, Clone, Copy)]
pub struct FabricSubscriptionSnapshotCodec {
    max_subscriptions: usize,
    max_snapshot_bytes: usize,
    max_string_bytes: usize,
    max_chunk_bytes: usize,
}

impl FabricSubscriptionSnapshotCodec {
    pub fn new(
        max_subscriptions: usize,
        max_snapshot_bytes: usize,
        max_string_bytes: usize,
        max_chunk_bytes: usize,
    ) -> Self {
        assert!(max_subscriptions > 0, "subscription bound must be non-zero");
        assert!(max_snapshot_bytes > 0, "snapshot bound must be non-zero");
        assert!(max_string_bytes > 0, "string bound must be non-zero");
        assert!(max_chunk_bytes > 0, "chunk bound must be non-zero");
        Self {
            max_subscriptions,
            max_snapshot_bytes,
            max_string_bytes,
            max_chunk_bytes,
        }
    }

    /// Encode an authoritative ordered subscription snapshot and split it into
    /// FAB1 chunks. Even an empty snapshot produces one chunk.
    pub fn encode_chunks(
        &self,
        owner: u64,
        generation: u64,
        subscriptions: &[FabricSubscriptionMetadataEntry],
    ) -> Result<Vec<FabricMetadataChunk>, FabricSubscriptionSnapshotError> {
        let payload = encode_subscription_metadata(
            subscriptions,
            self.max_subscriptions,
            self.max_snapshot_bytes,
            self.max_string_bytes,
        )?;
        Ok(chunk_snapshot(
            FabricMetadataKind::Subscriptions,
            owner,
            generation,
            &payload,
            self.max_chunk_bytes,
        )?)
    }
}

/// Receiver-side subscription assembler.
///
/// `push` returns `None` for every incomplete generation. A decoded snapshot is
/// returned only after the generic FAB1 assembler has verified the complete
/// generation hash, preserving the no-partial-routing-update invariant at the
/// API boundary used by live gossip integration.
#[derive(Debug)]
pub struct FabricSubscriptionSnapshotAssembler {
    metadata: FabricMetadataAssembler,
    max_subscriptions: usize,
    max_snapshot_bytes: usize,
    max_string_bytes: usize,
}

impl FabricSubscriptionSnapshotAssembler {
    pub fn new(
        max_subscriptions: usize,
        max_snapshot_bytes: usize,
        max_string_bytes: usize,
        max_chunks: u32,
        max_inflight_bytes: usize,
    ) -> Self {
        assert!(max_subscriptions > 0, "subscription bound must be non-zero");
        assert!(max_string_bytes > 0, "string bound must be non-zero");
        Self {
            metadata: FabricMetadataAssembler::with_limits(
                max_snapshot_bytes,
                max_chunks,
                64,
                max_inflight_bytes,
            ),
            max_subscriptions,
            max_snapshot_bytes,
            max_string_bytes,
        }
    }

    pub fn push(
        &mut self,
        chunk: FabricMetadataChunk,
    ) -> Result<Option<FabricSubscriptionSnapshot>, FabricSubscriptionSnapshotError> {
        if chunk.kind != FabricMetadataKind::Subscriptions {
            return Err(FabricSubscriptionSnapshotError::WrongMetadataKind);
        }

        let Some(snapshot) = self.metadata.push(chunk)? else {
            return Ok(None);
        };
        debug_assert_eq!(snapshot.kind, FabricMetadataKind::Subscriptions);

        let subscriptions = decode_subscription_metadata(
            &snapshot.payload,
            self.max_subscriptions,
            self.max_snapshot_bytes,
            self.max_string_bytes,
        )?;
        Ok(Some(FabricSubscriptionSnapshot {
            owner: snapshot.owner,
            generation: snapshot.generation,
            subscriptions,
        }))
    }

    pub fn discard_owner(&mut self, owner: u64) {
        self.metadata.discard_owner(owner);
    }

    pub fn inflight_snapshot_count(&self) -> usize {
        self.metadata.inflight_snapshot_count()
    }

    pub fn inflight_bytes(&self) -> usize {
        self.metadata.inflight_bytes()
    }
}
