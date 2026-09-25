//! Epoch-fenced ordered replication for the RESP cache.
//!
//! Replication reuses the cache placement epoch as its fencing term. Replica
//! application is strictly sequence-ordered and uses the same canonical
//! mutation representation as the local WAL.

use std::io;

use super::cache::CacheStore;
use super::cache_persistence::{
    apply_wal_mutation, decode_mutation, encode_mutation, CacheWalMutation,
};

const REPLICATION_MAGIC: &[u8; 8] = b"NLCREP01";
const REPLICATION_VERSION: u16 = 1;
const REPLICATION_HEADER_BYTES: usize = 8 + 2 + 8 + 8 + 4;
const CHECKSUM_BYTES: usize = 32;
const MAX_REPLICATION_PAYLOAD_BYTES: usize = 128 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheReplicationRecord {
    pub placement_epoch: u64,
    pub sequence: u64,
    pub mutation: CacheWalMutation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheReplicaApply {
    Applied { sequence: u64 },
    Duplicate { sequence: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheReplicaError {
    StaleEpoch { current: u64, received: u64 },
    FutureEpoch { current: u64, received: u64 },
    SequenceGap { expected: u64, received: u64 },
    SequenceExhausted,
    ApplyFailed(io::ErrorKind),
    Poisoned,
}

#[derive(Debug)]
pub struct CacheReplicaApplier {
    placement_epoch: u64,
    applied_sequence: u64,
    store: CacheStore,
    poisoned: bool,
}

impl CacheReplicaApplier {
    pub fn new(placement_epoch: u64, applied_sequence: u64, store: CacheStore) -> Self {
        Self {
            placement_epoch,
            applied_sequence,
            store,
            poisoned: false,
        }
    }

    pub fn placement_epoch(&self) -> u64 {
        self.placement_epoch
    }

    pub fn applied_sequence(&self) -> u64 {
        self.applied_sequence
    }

    pub fn is_poisoned(&self) -> bool {
        self.poisoned
    }

    pub fn store(&self) -> &CacheStore {
        &self.store
    }

    pub fn store_mut(&mut self) -> &mut CacheStore {
        &mut self.store
    }

    pub fn into_store(self) -> CacheStore {
        self.store
    }

    /// Advance the fencing epoch after the control plane has installed a newer
    /// placement snapshot. Equal epochs are idempotent; older epochs are
    /// rejected.
    pub fn fence_epoch(&mut self, proposed_epoch: u64) -> Result<(), CacheReplicaError> {
        if proposed_epoch < self.placement_epoch {
            return Err(CacheReplicaError::StaleEpoch {
                current: self.placement_epoch,
                received: proposed_epoch,
            });
        }
        self.placement_epoch = proposed_epoch;
        Ok(())
    }

    /// Apply exactly one canonical mutation from the current placement epoch.
    ///
    /// Records must be contiguous. Retransmissions at or below the already
    /// applied sequence are acknowledged as duplicates without executing the
    /// mutation again. A store-application error poisons the replica so it
    /// cannot silently continue from divergent state.
    pub fn apply(
        &mut self,
        record: &CacheReplicationRecord,
        store_now_ms: u64,
        wall_now_ms: u64,
    ) -> Result<CacheReplicaApply, CacheReplicaError> {
        if self.poisoned {
            return Err(CacheReplicaError::Poisoned);
        }
        if record.placement_epoch < self.placement_epoch {
            return Err(CacheReplicaError::StaleEpoch {
                current: self.placement_epoch,
                received: record.placement_epoch,
            });
        }
        if record.placement_epoch > self.placement_epoch {
            return Err(CacheReplicaError::FutureEpoch {
                current: self.placement_epoch,
                received: record.placement_epoch,
            });
        }
        if record.sequence <= self.applied_sequence {
            return Ok(CacheReplicaApply::Duplicate {
                sequence: record.sequence,
            });
        }

        let expected = self
            .applied_sequence
            .checked_add(1)
            .ok_or(CacheReplicaError::SequenceExhausted)?;
        if record.sequence != expected {
            return Err(CacheReplicaError::SequenceGap {
                expected,
                received: record.sequence,
            });
        }

        if let Err(error) =
            apply_wal_mutation(&mut self.store, &record.mutation, store_now_ms, wall_now_ms)
        {
            self.poisoned = true;
            return Err(CacheReplicaError::ApplyFailed(error.kind()));
        }

        self.applied_sequence = record.sequence;
        Ok(CacheReplicaApply::Applied {
            sequence: record.sequence,
        })
    }
}

pub fn encode_replication_frame(record: &CacheReplicationRecord) -> io::Result<Vec<u8>> {
    let payload = encode_mutation(&record.mutation)?;
    if payload.len() > MAX_REPLICATION_PAYLOAD_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "cache replication payload exceeds maximum size",
        ));
    }
    let payload_len = u32::try_from(payload.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "cache replication payload length exceeds u32",
        )
    })?;

    let mut bytes = Vec::with_capacity(REPLICATION_HEADER_BYTES + payload.len() + CHECKSUM_BYTES);
    bytes.extend_from_slice(REPLICATION_MAGIC);
    bytes.extend_from_slice(&REPLICATION_VERSION.to_le_bytes());
    bytes.extend_from_slice(&record.placement_epoch.to_le_bytes());
    bytes.extend_from_slice(&record.sequence.to_le_bytes());
    bytes.extend_from_slice(&payload_len.to_le_bytes());
    bytes.extend_from_slice(&payload);

    let checksum = blake3::hash(&bytes);
    bytes.extend_from_slice(checksum.as_bytes());
    Ok(bytes)
}

pub fn decode_replication_frame(bytes: &[u8]) -> io::Result<CacheReplicationRecord> {
    if bytes.len() < REPLICATION_HEADER_BYTES + CHECKSUM_BYTES {
        return Err(invalid_data("cache replication frame is truncated"));
    }

    let body_len = bytes.len() - CHECKSUM_BYTES;
    let (body, stored_checksum) = bytes.split_at(body_len);
    let checksum = blake3::hash(body);
    if checksum.as_bytes() != stored_checksum {
        return Err(invalid_data("cache replication checksum mismatch"));
    }
    if &body[..8] != REPLICATION_MAGIC {
        return Err(invalid_data("invalid cache replication magic"));
    }

    let version = u16::from_le_bytes([body[8], body[9]]);
    if version != REPLICATION_VERSION {
        return Err(invalid_data("unsupported cache replication version"));
    }

    let placement_epoch = u64::from_le_bytes(
        body[10..18]
            .try_into()
            .expect("fixed replication epoch slice"),
    );
    let sequence = u64::from_le_bytes(
        body[18..26]
            .try_into()
            .expect("fixed replication sequence slice"),
    );
    let payload_len = u32::from_le_bytes(
        body[26..30]
            .try_into()
            .expect("fixed replication payload-length slice"),
    ) as usize;
    if payload_len > MAX_REPLICATION_PAYLOAD_BYTES {
        return Err(invalid_data("cache replication payload exceeds maximum size"));
    }

    let expected_len = REPLICATION_HEADER_BYTES
        .checked_add(payload_len)
        .ok_or_else(|| invalid_data("cache replication frame length overflow"))?;
    if body.len() != expected_len {
        return Err(invalid_data("cache replication frame length mismatch"));
    }

    let mutation = decode_mutation(&body[REPLICATION_HEADER_BYTES..])?;
    Ok(CacheReplicationRecord {
        placement_epoch,
        sequence,
        mutation,
    })
}

fn invalid_data(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{
        CacheConfig, CacheEvictionPolicy, CacheShardOwner, CacheStore, CacheValueView,
        CacheWalMutation,
    };

    fn integer_record(epoch: u64, sequence: u64, key: &[u8], value: i64) -> CacheReplicationRecord {
        CacheReplicationRecord {
            placement_epoch: epoch,
            sequence,
            mutation: CacheWalMutation::SetInteger {
                key: key.to_vec(),
                value,
                expires_unix_ms: None,
            },
        }
    }

    #[test]
    fn replication_frame_round_trips_with_epoch_sequence_and_checksum() {
        let record = CacheReplicationRecord {
            placement_epoch: 41,
            sequence: 9001,
            mutation: CacheWalMutation::SetBytes {
                key: b"tenant:{42}:key".to_vec(),
                value: b"value".to_vec(),
                expires_unix_ms: Some(123_456_789),
            },
        };

        let encoded = encode_replication_frame(&record).unwrap();
        assert_eq!(decode_replication_frame(&encoded).unwrap(), record);
    }

    #[test]
    fn replication_frame_rejects_checksum_corruption() {
        let record = integer_record(7, 1, b"k", 1);
        let mut encoded = encode_replication_frame(&record).unwrap();
        let payload_byte = encoded.len() - 33;
        encoded[payload_byte] ^= 0x01;

        let error = decode_replication_frame(&encoded).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn replica_applies_only_contiguous_records_for_current_epoch() {
        let mut replica = CacheReplicaApplier::new(
            11,
            0,
            CacheStore::with_config_and_eviction(
                CacheConfig::default(),
                CacheEvictionPolicy::S3Fifo,
            ),
        );

        assert_eq!(
            replica.apply(&integer_record(11, 1, b"k", 1), 0, 1_000).unwrap(),
            CacheReplicaApply::Applied { sequence: 1 }
        );
        assert_eq!(
            replica.apply(&integer_record(11, 2, b"k", 2), 0, 1_000).unwrap(),
            CacheReplicaApply::Applied { sequence: 2 }
        );
        assert_eq!(replica.applied_sequence(), 2);
        assert_eq!(
            replica.store_mut().get(b"k", 0),
            Some(CacheValueView::Integer(2))
        );
    }

    #[test]
    fn replica_rejects_sequence_gap_without_mutating_store() {
        let mut replica = CacheReplicaApplier::new(4, 10, CacheStore::new());
        let error = replica
            .apply(&integer_record(4, 12, b"k", 12), 0, 1_000)
            .unwrap_err();

        assert_eq!(
            error,
            CacheReplicaError::SequenceGap {
                expected: 11,
                received: 12,
            }
        );
        assert_eq!(replica.applied_sequence(), 10);
        assert_eq!(replica.store_mut().get(b"k", 0), None);
    }

    #[test]
    fn epoch_fence_rejects_old_primary_records() {
        let mut replica = CacheReplicaApplier::new(8, 3, CacheStore::new());
        replica.fence_epoch(9).unwrap();

        let error = replica
            .apply(&integer_record(8, 4, b"k", 4), 0, 1_000)
            .unwrap_err();
        assert_eq!(
            error,
            CacheReplicaError::StaleEpoch {
                current: 9,
                received: 8,
            }
        );
        assert_eq!(replica.applied_sequence(), 3);
    }

    #[test]
    fn future_epoch_requires_control_plane_fence_before_application() {
        let mut replica = CacheReplicaApplier::new(8, 3, CacheStore::new());

        let error = replica
            .apply(&integer_record(9, 4, b"k", 4), 0, 1_000)
            .unwrap_err();
        assert_eq!(
            error,
            CacheReplicaError::FutureEpoch {
                current: 8,
                received: 9,
            }
        );

        replica.fence_epoch(9).unwrap();
        assert_eq!(
            replica.apply(&integer_record(9, 4, b"k", 4), 0, 1_000).unwrap(),
            CacheReplicaApply::Applied { sequence: 4 }
        );
    }

    #[test]
    fn retransmitted_applied_record_is_idempotent() {
        let mut replica = CacheReplicaApplier::new(3, 0, CacheStore::new());
        let record = integer_record(3, 1, b"counter", 5);

        assert_eq!(
            replica.apply(&record, 0, 1_000).unwrap(),
            CacheReplicaApply::Applied { sequence: 1 }
        );
        assert_eq!(
            replica.apply(&record, 0, 1_000).unwrap(),
            CacheReplicaApply::Duplicate { sequence: 1 }
        );
        assert_eq!(
            replica.store_mut().get(b"counter", 0),
            Some(CacheValueView::Integer(5))
        );
    }

    #[test]
    fn replica_ack_frame_round_trips_identity_epoch_and_sequence() {
        let ack = CacheReplicaAck {
            placement_epoch: 13,
            replica: CacheShardOwner {
                node_id: 44,
                shard: 2,
            },
            applied_sequence: 808,
        };

        let encoded = encode_replica_ack(&ack);
        assert_eq!(decode_replica_ack(&encoded).unwrap(), ack);
    }

    #[test]
    fn replica_ack_tracker_counts_distinct_replicas_at_or_above_sequence() {
        let first = CacheShardOwner {
            node_id: 2,
            shard: 0,
        };
        let second = CacheShardOwner {
            node_id: 3,
            shard: 0,
        };
        let mut tracker = CacheReplicaAckTracker::new(5, &[first, second]);

        assert_eq!(
            tracker
                .observe(
                    &CacheReplicaAck {
                        placement_epoch: 5,
                        replica: first,
                        applied_sequence: 10,
                    },
                    10,
                )
                .unwrap(),
            CacheReplicaAckObservation::Advanced {
                replica: first,
                sequence: 10,
            }
        );
        assert_eq!(tracker.acked_replicas(10), 1);
        assert!(!tracker.satisfies(10, 2));

        tracker
            .observe(
                &CacheReplicaAck {
                    placement_epoch: 5,
                    replica: second,
                    applied_sequence: 10,
                },
                10,
            )
            .unwrap();
        assert_eq!(tracker.acked_replicas(10), 2);
        assert!(tracker.satisfies(10, 2));
    }

    #[test]
    fn replica_ack_cannot_claim_sequence_primary_has_not_produced() {
        let replica = CacheShardOwner {
            node_id: 2,
            shard: 0,
        };
        let mut tracker = CacheReplicaAckTracker::new(5, &[replica]);
        let error = tracker
            .observe(
                &CacheReplicaAck {
                    placement_epoch: 5,
                    replica,
                    applied_sequence: 11,
                },
                10,
            )
            .unwrap_err();

        assert_eq!(
            error,
            CacheReplicaAckError::FutureSequence {
                local: 10,
                received: 11,
            }
        );
        assert_eq!(tracker.acked_replicas(1), 0);
    }

    #[test]
    fn replica_ack_is_monotonic_per_replica() {
        let replica = CacheShardOwner {
            node_id: 2,
            shard: 0,
        };
        let mut tracker = CacheReplicaAckTracker::new(5, &[replica]);
        tracker
            .observe(
                &CacheReplicaAck {
                    placement_epoch: 5,
                    replica,
                    applied_sequence: 9,
                },
                10,
            )
            .unwrap();

        assert_eq!(
            tracker
                .observe(
                    &CacheReplicaAck {
                        placement_epoch: 5,
                        replica,
                        applied_sequence: 8,
                    },
                    10,
                )
                .unwrap(),
            CacheReplicaAckObservation::Duplicate {
                replica,
                sequence: 9,
            }
        );
        assert_eq!(tracker.acked_replicas(9), 1);
    }

    #[test]
    fn epoch_fence_clears_old_acknowledgements_and_rejects_old_epoch() {
        let replica = CacheShardOwner {
            node_id: 2,
            shard: 0,
        };
        let mut tracker = CacheReplicaAckTracker::new(5, &[replica]);
        tracker
            .observe(
                &CacheReplicaAck {
                    placement_epoch: 5,
                    replica,
                    applied_sequence: 10,
                },
                10,
            )
            .unwrap();
        assert!(tracker.satisfies(10, 1));

        tracker.fence_epoch(6).unwrap();
        assert!(!tracker.satisfies(10, 1));

        let error = tracker
            .observe(
                &CacheReplicaAck {
                    placement_epoch: 5,
                    replica,
                    applied_sequence: 10,
                },
                10,
            )
            .unwrap_err();
        assert_eq!(
            error,
            CacheReplicaAckError::StaleEpoch {
                current: 6,
                received: 5,
            }
        );
    }

    #[test]
    fn bootstrap_state_sets_epoch_and_resume_sequence() {
        let mut store = CacheStore::new();
        store.set_integer(b"k", 99, None, 0);

        let mut replica = CacheReplicaApplier::new(21, 500, store);
        assert_eq!(replica.placement_epoch(), 21);
        assert_eq!(replica.applied_sequence(), 500);
        assert_eq!(
            replica.store_mut().get(b"k", 0),
            Some(CacheValueView::Integer(99))
        );
    }
}
