//! Logical Redis-slot placement for the Nulang cache tier.
//!
//! The hot path is a single indexed read: 16,384 Redis logical slots map to a
//! physical owner (node + shard). Placement mutation is a control-plane action
//! applied in epoch-fenced batches after full validation, so readers never
//! observe a partially validated topology.

use super::cache::{redis_slot, REDIS_CLUSTER_SLOTS};

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

/// Two-phase slot movement. The stable owner remains `source` until commit.
///
/// Every node can carry the same transition snapshot. The source interprets it
/// as MIGRATING; the target interprets it as IMPORTING. That avoids publishing
/// node-relative migration state and makes epoch fencing deterministic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheSlotMigration {
    pub source: CacheShardOwner,
    pub target: CacheShardOwner,
    pub started_epoch: u64,
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
    InvalidSlot(u16),
    MigrationInProgress(u16),
    MigrationNotFound(u16),
    MigrationSourceMismatch {
        slot: u16,
        expected: CacheShardOwner,
        actual: CacheShardOwner,
    },
    MigrationTargetMatchesSource(u16),
    MigrationMismatch {
        slot: u16,
        expected_source: CacheShardOwner,
        expected_target: CacheShardOwner,
        actual_source: CacheShardOwner,
        actual_target: CacheShardOwner,
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
    migrations: Vec<Option<CacheSlotMigration>>,
}

impl CacheSlotMap {
    /// Build a local-only placement with balanced contiguous slot ranges.
    ///
    /// CRC16 already spreads ordinary keys uniformly over the 16,384 logical
    /// slots. Keeping each physical shard's default ownership contiguous makes
    /// Redis Cluster topology compact and slot migration tractable without
    /// sacrificing expected key distribution.
    pub fn new_local(node_id: u64, shard_count: u16) -> Result<Self, CachePlacementError> {
        if shard_count == 0 || shard_count > REDIS_CLUSTER_SLOTS {
            return Err(CachePlacementError::InvalidShardCount);
        }

        let mut owners = Vec::with_capacity(REDIS_CLUSTER_SLOTS as usize);
        for slot in 0..REDIS_CLUSTER_SLOTS {
            let shard =
                (u32::from(slot) * u32::from(shard_count) / u32::from(REDIS_CLUSTER_SLOTS)) as u16;
            owners.push(CacheShardOwner { node_id, shard });
        }

        Ok(Self {
            epoch: 0,
            owners,
            migrations: vec![None; REDIS_CLUSTER_SLOTS as usize],
        })
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn owner_for_slot(&self, slot: u16) -> Option<CacheShardOwner> {
        self.owners.get(slot as usize).copied()
    }

    pub fn migration_for_slot(&self, slot: u16) -> Option<CacheSlotMigration> {
        self.migrations.get(slot as usize).copied().flatten()
    }

    pub fn migrations(&self) -> impl Iterator<Item = (u16, CacheSlotMigration)> + '_ {
        self.migrations
            .iter()
            .enumerate()
            .filter_map(|(slot, migration)| migration.map(|migration| (slot as u16, migration)))
    }

    pub fn owner_for_key(&self, key: &[u8]) -> CacheShardOwner {
        // redis_slot always returns 0..16383, so this index is guaranteed.
        self.owners[redis_slot(key) as usize]
    }

    /// Return contiguous logical-slot ranges in ascending slot order.
    ///
    /// Topology commands use this cold-path view to describe ownership
    /// without exposing or copying the full 16,384-entry table.
    pub fn slot_ranges(&self) -> Vec<CacheSlotRange> {
        let mut ranges = Vec::new();
        if self.owners.is_empty() {
            return ranges;
        }

        let mut start = 0u16;
        let mut owner = self.owners[0];

        for slot in 1..REDIS_CLUSTER_SLOTS {
            let next = self.owners[slot as usize];
            if next != owner {
                ranges.push(CacheSlotRange {
                    start,
                    end: slot - 1,
                    owner,
                });
                start = slot;
                owner = next;
            }
        }

        ranges.push(CacheSlotRange {
            start,
            end: REDIS_CLUSTER_SLOTS - 1,
            owner,
        });
        ranges
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
        self.require_new_epoch(proposed_epoch)?;

        for assignment in assignments {
            if assignment.start > assignment.end || assignment.end >= REDIS_CLUSTER_SLOTS {
                return Err(CachePlacementError::InvalidRange {
                    start: assignment.start,
                    end: assignment.end,
                });
            }
        }

        for assignment in assignments {
            for slot in assignment.start..=assignment.end {
                if self.migrations[slot as usize].is_some() {
                    return Err(CachePlacementError::MigrationInProgress(slot));
                }
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

    /// Start a two-phase slot migration without changing stable ownership.
    ///
    /// `expected_source` fences stale controllers that planned against an old
    /// topology snapshot. The target may begin accepting one-shot ASKING
    /// traffic immediately, while the source remains the advertised owner.
    pub fn begin_migration(
        &mut self,
        proposed_epoch: u64,
        slot: u16,
        expected_source: CacheShardOwner,
        target: CacheShardOwner,
    ) -> Result<(), CachePlacementError> {
        self.require_new_epoch(proposed_epoch)?;
        let Some(&actual_source) = self.owners.get(slot as usize) else {
            return Err(CachePlacementError::InvalidSlot(slot));
        };
        if self.migrations[slot as usize].is_some() {
            return Err(CachePlacementError::MigrationInProgress(slot));
        }
        if actual_source != expected_source {
            return Err(CachePlacementError::MigrationSourceMismatch {
                slot,
                expected: expected_source,
                actual: actual_source,
            });
        }
        if target == actual_source {
            return Err(CachePlacementError::MigrationTargetMatchesSource(slot));
        }

        self.migrations[slot as usize] = Some(CacheSlotMigration {
            source: actual_source,
            target,
            started_epoch: proposed_epoch,
        });
        self.epoch = proposed_epoch;
        Ok(())
    }

    /// Commit a migration and atomically make the target the stable owner.
    pub fn commit_migration(
        &mut self,
        proposed_epoch: u64,
        slot: u16,
        expected_source: CacheShardOwner,
        expected_target: CacheShardOwner,
    ) -> Result<(), CachePlacementError> {
        self.require_new_epoch(proposed_epoch)?;
        let Some(current) = self.migration_for_slot(slot) else {
            if slot >= REDIS_CLUSTER_SLOTS {
                return Err(CachePlacementError::InvalidSlot(slot));
            }
            return Err(CachePlacementError::MigrationNotFound(slot));
        };
        if current.source != expected_source || current.target != expected_target {
            return Err(CachePlacementError::MigrationMismatch {
                slot,
                expected_source,
                expected_target,
                actual_source: current.source,
                actual_target: current.target,
            });
        }
        let actual_owner = self.owners[slot as usize];
        if actual_owner != expected_source {
            return Err(CachePlacementError::MigrationSourceMismatch {
                slot,
                expected: expected_source,
                actual: actual_owner,
            });
        }

        self.owners[slot as usize] = expected_target;
        self.migrations[slot as usize] = None;
        self.epoch = proposed_epoch;
        Ok(())
    }

    /// Abort a migration while keeping the source as stable owner.
    pub fn cancel_migration(
        &mut self,
        proposed_epoch: u64,
        slot: u16,
        expected_source: CacheShardOwner,
        expected_target: CacheShardOwner,
    ) -> Result<(), CachePlacementError> {
        self.require_new_epoch(proposed_epoch)?;
        let Some(current) = self.migration_for_slot(slot) else {
            if slot >= REDIS_CLUSTER_SLOTS {
                return Err(CachePlacementError::InvalidSlot(slot));
            }
            return Err(CachePlacementError::MigrationNotFound(slot));
        };
        if current.source != expected_source || current.target != expected_target {
            return Err(CachePlacementError::MigrationMismatch {
                slot,
                expected_source,
                expected_target,
                actual_source: current.source,
                actual_target: current.target,
            });
        }

        self.migrations[slot as usize] = None;
        self.epoch = proposed_epoch;
        Ok(())
    }

    fn require_new_epoch(&self, proposed_epoch: u64) -> Result<(), CachePlacementError> {
        if proposed_epoch <= self.epoch {
            return Err(CachePlacementError::StaleEpoch {
                current: self.epoch,
                proposed: proposed_epoch,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_map_distributes_balanced_contiguous_ranges() {
        let map = CacheSlotMap::new_local(7, 4).unwrap();

        assert_eq!(
            map.owner_for_slot(0),
            Some(CacheShardOwner {
                node_id: 7,
                shard: 0
            })
        );
        assert_eq!(
            map.owner_for_slot(4_095),
            Some(CacheShardOwner {
                node_id: 7,
                shard: 0
            })
        );
        assert_eq!(
            map.owner_for_slot(4_096),
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
        assert_eq!(map.slot_ranges().len(), 4);
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
    fn slot_ranges_coalesce_adjacent_owners() {
        let mut map = CacheSlotMap::new_local(1, 1).unwrap();
        map.apply_epoch(
            1,
            &[CacheSlotRange {
                start: 100,
                end: 199,
                owner: CacheShardOwner {
                    node_id: 2,
                    shard: 7,
                },
            }],
        )
        .unwrap();

        assert_eq!(
            map.slot_ranges(),
            vec![
                CacheSlotRange {
                    start: 0,
                    end: 99,
                    owner: CacheShardOwner {
                        node_id: 1,
                        shard: 0,
                    },
                },
                CacheSlotRange {
                    start: 100,
                    end: 199,
                    owner: CacheShardOwner {
                        node_id: 2,
                        shard: 7,
                    },
                },
                CacheSlotRange {
                    start: 200,
                    end: REDIS_CLUSTER_SLOTS - 1,
                    owner: CacheShardOwner {
                        node_id: 1,
                        shard: 0,
                    },
                },
            ]
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
    fn migration_is_epoch_fenced_and_keeps_source_owner_until_commit() {
        let mut map = CacheSlotMap::new_local(1, 2).unwrap();
        let slot = 1234;
        let source = map.owner_for_slot(slot).unwrap();
        let target = CacheShardOwner {
            node_id: 9,
            shard: 3,
        };

        map.begin_migration(1, slot, source, target).unwrap();

        assert_eq!(map.epoch(), 1);
        assert_eq!(map.owner_for_slot(slot), Some(source));
        assert_eq!(
            map.migration_for_slot(slot),
            Some(CacheSlotMigration {
                source,
                target,
                started_epoch: 1,
            })
        );
        assert_eq!(
            map.begin_migration(1, slot, source, target),
            Err(CachePlacementError::StaleEpoch {
                current: 1,
                proposed: 1,
            })
        );

        map.commit_migration(2, slot, source, target).unwrap();
        assert_eq!(map.epoch(), 2);
        assert_eq!(map.owner_for_slot(slot), Some(target));
        assert_eq!(map.migration_for_slot(slot), None);
    }

    #[test]
    fn generic_epoch_assignment_cannot_bypass_active_migration() {
        let mut map = CacheSlotMap::new_local(1, 1).unwrap();
        let slot = 77;
        let source = map.owner_for_slot(slot).unwrap();
        let target = CacheShardOwner {
            node_id: 2,
            shard: 0,
        };
        map.begin_migration(1, slot, source, target).unwrap();

        assert_eq!(
            map.apply_epoch(
                2,
                &[CacheSlotRange {
                    start: slot,
                    end: slot,
                    owner: target,
                }],
            ),
            Err(CachePlacementError::MigrationInProgress(slot))
        );
        assert_eq!(map.epoch(), 1);
        assert_eq!(map.owner_for_slot(slot), Some(source));
    }

    #[test]
    fn stale_or_mismatched_migration_controller_cannot_commit() {
        let mut map = CacheSlotMap::new_local(1, 1).unwrap();
        let slot = 88;
        let source = map.owner_for_slot(slot).unwrap();
        let target = CacheShardOwner {
            node_id: 2,
            shard: 0,
        };
        let wrong_target = CacheShardOwner {
            node_id: 3,
            shard: 0,
        };

        map.begin_migration(4, slot, source, target).unwrap();

        assert!(matches!(
            map.commit_migration(5, slot, source, wrong_target),
            Err(CachePlacementError::MigrationMismatch { .. })
        ));
        assert_eq!(map.epoch(), 4);
        assert_eq!(map.owner_for_slot(slot), Some(source));
        assert_eq!(map.migration_for_slot(slot).unwrap().target, target);
    }

    #[test]
    fn invalid_shard_counts_are_rejected() {
        assert!(matches!(
            CacheSlotMap::new_local(1, 0),
            Err(CachePlacementError::InvalidShardCount)
        ));
        assert!(matches!(
            CacheSlotMap::new_local(1, REDIS_CLUSTER_SLOTS + 1),
            Err(CachePlacementError::InvalidShardCount)
        ));
    }
}
