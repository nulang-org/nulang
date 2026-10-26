//! Conservative, non-mutating retention *planning* for one tablet.
//!
//! This layer deliberately does not delete WAL frames, compact MVCC versions,
//! or manage consumer lifecycles. Every source starts Unknown and blocks its
//! respective decision until an authoritative owner explicitly certifies
//! coverage. A returned bound is a necessary limit, NOT sufficient evidence
//! that physical reclamation is safe: WAL/checkpoint pairing, durable leases,
//! active readers, atomic consumer publication and crash recovery need their
//! own admission checks before any deletion is wired up.
//!
//! MVCC snapshots and WAL cursors use distinct semantics. A snapshot at S
//! requires the most recent per-key version at or before S, whereas a WAL
//! consumer cursor N needs log entries starting at N. Do not use a snapshot
//! floor as an unconditional "drop versions before S" instruction.

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RetentionClass {
    Snapshot,
    Changefeed,
    Backup,
    Replica,
    Branch,
}

impl RetentionClass {
    const WAL_CLASSES: [Self; 4] = [Self::Changefeed, Self::Backup, Self::Replica, Self::Branch];
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Coverage {
    /// The source has not proved that all its consumers are accounted for.
    Unknown,
    /// The source has explicitly certified that it has no outstanding pins.
    None,
    /// Snapshot: oldest visible read sequence, WAL: first record still needed.
    Earliest(u64),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetentionBlock {
    Unknown(Vec<RetentionClass>),
    NoCheckpoint,
    CheckpointAhead {
        checkpoint: u64,
        durable_tail: u64,
    },
    OutOfRange {
        source: RetentionClass,
        sequence: u64,
        durable_tail: u64,
    },
}

/// One tablet's conservative, explicit retention inputs. These fields are
/// private so each source can be independently certified, and Unknown is the
/// default. This is *not* a durable registry or lease manager.
#[derive(Debug, Clone)]
pub struct RetentionPolicy {
    snapshot: Coverage,
    changefeed: Coverage,
    backup: Coverage,
    replica: Coverage,
    branch: Coverage,
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            snapshot: Coverage::Unknown,
            changefeed: Coverage::Unknown,
            backup: Coverage::Unknown,
            replica: Coverage::Unknown,
            branch: Coverage::Unknown,
        }
    }
}

impl RetentionPolicy {
    fn coverage(&self, source: RetentionClass) -> Coverage {
        match source {
            RetentionClass::Snapshot => self.snapshot,
            RetentionClass::Changefeed => self.changefeed,
            RetentionClass::Backup => self.backup,
            RetentionClass::Replica => self.replica,
            RetentionClass::Branch => self.branch,
        }
    }

    /// Supply a coverage assertion from the corresponding subsystem.
    /// Callers must not assert None based solely on missing in-memory state:
    /// disconnected durable consumers and in-flight readers must be counted.
    pub fn set_coverage(&mut self, source: RetentionClass, coverage: Coverage) {
        match source {
            RetentionClass::Snapshot => self.snapshot = coverage,
            RetentionClass::Changefeed => self.changefeed = coverage,
            RetentionClass::Backup => self.backup = coverage,
            RetentionClass::Replica => self.replica = coverage,
            RetentionClass::Branch => self.branch = coverage,
        }
    }

    /// Returns the oldest live snapshot, or None only when the snapshot owner
    /// has certified no readers. The compaction algorithm must preserve the
    /// predecessor version visible at this floor, not discard every older row.
    pub fn mvcc_snapshot_floor(&self, durable_tail: u64) -> Result<Option<u64>, RetentionBlock> {
        match self.snapshot {
            Coverage::Unknown => Err(RetentionBlock::Unknown(vec![RetentionClass::Snapshot])),
            Coverage::None => Ok(None),
            Coverage::Earliest(sequence) if sequence <= durable_tail => Ok(Some(sequence)),
            Coverage::Earliest(sequence) => Err(RetentionBlock::OutOfRange {
                source: RetentionClass::Snapshot,
                sequence,
                durable_tail,
            }),
        }
    }

    /// Returns a nonnegative, inclusive maximum WAL sequence that might be
    /// reclaimable after *separate* crash-safe checkpoint and writer checks.
    ///
    /// Unknown sources block. A cursor N keeps the record at N, therefore this
    /// bound cannot exceed N-1. Even with no consumers, checkpoint_sequence is
    /// a hard upper bound. No physical mutation is performed.
    pub fn wal_reclaim_bound(
        &self,
        checkpoint_sequence: Option<u64>,
        durable_tail: u64,
    ) -> Result<u64, RetentionBlock> {
        let unknown: Vec<_> = RetentionClass::WAL_CLASSES
            .into_iter()
            .filter(|&class| self.coverage(class) == Coverage::Unknown)
            .collect();
        if !unknown.is_empty() {
            return Err(RetentionBlock::Unknown(unknown));
        }
        let checkpoint = checkpoint_sequence.ok_or(RetentionBlock::NoCheckpoint)?;
        if checkpoint > durable_tail {
            return Err(RetentionBlock::CheckpointAhead {
                checkpoint,
                durable_tail,
            });
        }

        let mut bound = checkpoint;
        for source in RetentionClass::WAL_CLASSES {
            if let Coverage::Earliest(sequence) = self.coverage(source) {
                // This allows tail+1 to represent a fully consumed log, while
                // avoiding overflow when durable_tail is u64::MAX.
                if sequence == 0 || (sequence > durable_tail && sequence - durable_tail != 1) {
                    return Err(RetentionBlock::OutOfRange {
                        source,
                        sequence,
                        durable_tail,
                    });
                }
                bound = bound.min(sequence - 1);
            }
        }
        Ok(bound)
    }
}
