//! Logical Redis-slot placement for the Nulang cache tier.
//!
//! The hot path is a single indexed read: 16,384 Redis logical slots map to a
//! physical owner (node + shard). Placement mutation is a control-plane action
//! applied in epoch-fenced batches after full validation, so readers never
//! observe a partially validated topology.

use super::cache::{default_physical_shard, redis_slot, REDIS_CLUSTER_SLOTS};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CacheShardOwner {
    pub node_id: u64,
    pub shard: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheSlotRange {
    pub start: u16,
    pub end: u16,
    pub owner: CacheShardOwner,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CachePlacementError {
    InvalidShardCount,
    InvalidRange {
        start: u16,
        end: u16,
    },
    OverlappingRange {
        first_start: u16,
        first_end: u16,
        second_start: u16,
        second_end: u16,
    },
    StaleEpoch {
        current: u64,
        proposed: u64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheRoute {
    Local { slot: u16, shard: u16 },
    Remote { slot: u16, node_id: u64, shard: u16 },
}

/// Placement snapshot for the complete Redis logical-slot space.
///
/// Instances are cheap to read and intentionally require `&mut self` for
/// topology changes. A server can publish an updated snapshot behind whatever
/// pointer-swap mechanism it chooses without putting synchronization inside the
/// lookup itself.
#[derive(Debug, Clone)]
pub struct CacheSlotMap {
    epoch: u64,
    owners: Vec<CacheShardOwner>,
}

impl CacheSlotMap {
    /// Build a local-only placement by distributing logical slots modulo the
    /// number of physical cache shards.
    pub fn new_local(node_id: u64, shard_count: u16) -> Result<Self, CachePlacementError> {
        if shard_count == 0 {
            return Err(CachePlacementError::InvalidShardCount);
        }

        let mut owners = Vec::with_capacity(REDIS_CLUSTER_SLOTS as usize);
        for slot in 0..REDIS_CLUSTER_SLOTS {
            owners.push(CacheShardOwner {
                node_id,
                shard: default_physical_shard(slot, shard_count as usize) as u16,
            });
        }

        Ok(Self { epoch: 0, owners })
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn owner_for_slot(&self, slot: u16) -> Option<CacheShardOwner> {
        self.owners.get(slot as usize).copied()
    }

    pub fn owner_for_key(&self, key: &[u8]) -> CacheShardOwner {
        // redis_slot always returns 0..16383, so this index is guaranteed.
        self.owners[redis_slot(key) as usize]
    }

    pub fn route_key(&self, local_node_id: u64, key: &[u8]) -> CacheRoute {
        let slot = redis_slot(key);
        let owner = self.owners[slot as usize];
        if owner.node_id == local_node_id {
            CacheRoute::Local {
                slot,
                shard: owner.shard,
            }
        } else {
            CacheRoute::Remote {
                slot,
                node_id: owner.node_id,
                shard: owner.shard,
            }
        }
    }

    /// Apply a control-plane placement epoch.
    ///
    /// Every range is validated before any owner entry is mutated. Ranges in
    /// the same epoch may not overlap; callers must provide one unambiguous
    /// owner for each slot they change.
    pub fn apply_epoch(
        &mut self,
        proposed_epoch: u64,
        assignments: &[CacheSlotRange],
    ) -> Result<(), CachePlacementError> {
        if proposed_epoch <= self.epoch {
            return Err(CachePlacementError::StaleEpoch {
                current: self.epoch,
                proposed: proposed_epoch,
            });
        }

        for assignment in assignments {
            if assignment.start > assignment.end || assignment.end >= REDIS_CLUSTER_SLOTS {
                return Err(CachePlacementError::InvalidRange {
                    start: assignment.start,
                    end: assignment.end,
                });
            }
        }

        for (idx, first) in assignments.iter().enumerate() {
            for second in assignments.iter().skip(idx + 1) {
                if first.start <= second.end && second.start <= first.end {
                    return Err(CachePlacementError::OverlappingRange {
                        first_start: first.start,
                        first_end: first.end,
                        second_start: second.start,
                        second_end: second.end,
                    });
                }
            }
        }

        for assignment in assignments {
            for slot in assignment.start..=assignment.end {
                self.owners[slot as usize] = assignment.owner;
            }
        }
        self.epoch = proposed_epoch;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_map_distributes_all_slots_across_shards() {
        let map = CacheSlotMap::new_local(7, 4).unwrap();

        assert_eq!(
            map.owner_for_slot(0),
            Some(CacheShardOwner {
                node_id: 7,
                shard: 0
            })
        );
        assert_eq!(
            map.owner_for_slot(1),
            Some(CacheShardOwner {
                node_id: 7,
                shard: 1
            })
        );
        assert_eq!(
            map.owner_for_slot(16_383),
            Some(CacheShardOwner {
                node_id: 7,
                shard: 3
            })
        );
        assert_eq!(map.owner_for_slot(16_384), None);
    }

    #[test]
    fn redis_hash_tags_route_related_keys_to_same_owner() {
        let map = CacheSlotMap::new_local(1, 8).unwrap();
        assert_eq!(
            map.owner_for_key(b"tenant:{42}:profile"),
            map.owner_for_key(b"tenant:{42}:sessions")
        );
    }

    #[test]
    fn epoch_update_remaps_range_and_routes_remote_owner() {
        let mut map = CacheSlotMap::new_local(1, 4).unwrap();
        let slot = redis_slot(b"remote-key");

        map.apply_epoch(
            1,
            &[CacheSlotRange {
                start: slot,
                end: slot,
                owner: CacheShardOwner {
                    node_id: 9,
                    shard: 2,
                },
            }],
        )
        .unwrap();

        assert_eq!(map.epoch(), 1);
        assert_eq!(
            map.route_key(1, b"remote-key"),
            CacheRoute::Remote {
                slot,
                node_id: 9,
                shard: 2
            }
        );
    }

    #[test]
    fn stale_epoch_is_rejected_without_mutation() {
        let mut map = CacheSlotMap::new_local(1, 2).unwrap();
        let before = map.owner_for_slot(10).unwrap();

        map.apply_epoch(
            2,
            &[CacheSlotRange {
                start: 10,
                end: 10,
                owner: CacheShardOwner {
                    node_id: 2,
                    shard: 0,
                },
            }],
        )
        .unwrap();

        let installed = map.owner_for_slot(10).unwrap();
        assert_ne!(installed, before);
        assert_eq!(
            map.apply_epoch(
                2,
                &[CacheSlotRange {
                    start: 10,
                    end: 10,
                    owner: before,
                }],
            ),
            Err(CachePlacementError::StaleEpoch {
                current: 2,
                proposed: 2
            })
        );
        assert_eq!(map.owner_for_slot(10), Some(installed));
    }

    #[test]
    fn overlapping_epoch_is_rejected_before_any_slot_changes() {
        let mut map = CacheSlotMap::new_local(1, 2).unwrap();
        let before_10 = map.owner_for_slot(10);
        let before_12 = map.owner_for_slot(12);

        let result = map.apply_epoch(
            1,
            &[
                CacheSlotRange {
                    start: 10,
                    end: 12,
                    owner: CacheShardOwner {
                        node_id: 2,
                        shard: 0,
                    },
                },
                CacheSlotRange {
                    start: 12,
                    end: 14,
                    owner: CacheShardOwner {
                        node_id: 3,
                        shard: 1,
                    },
                },
            ],
        );

        assert!(matches!(
            result,
            Err(CachePlacementError::OverlappingRange { .. })
        ));
        assert_eq!(map.epoch(), 0);
        assert_eq!(map.owner_for_slot(10), before_10);
        assert_eq!(map.owner_for_slot(12), before_12);
    }

    #[test]
    fn zero_shards_are_rejected() {
        assert!(matches!(
            CacheSlotMap::new_local(1, 0),
            Err(CachePlacementError::InvalidShardCount)
        ));
    }
}
