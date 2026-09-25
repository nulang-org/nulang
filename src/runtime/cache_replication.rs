//! Cache replication protocol and ordered replica application.
//!
//! Tests are intentionally written first. The implementation below this test
//! contract is added in the following commit.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{
        CacheConfig, CacheEvictionPolicy, CacheStore, CacheValueView, CacheWalMutation,
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
