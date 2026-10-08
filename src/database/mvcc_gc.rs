//! Pure MVCC retention rules for future NuDB compaction garbage collection.
//!
//! This module decides which versions are semantically required for readers at
//! or above a retention floor. It does not mutate SSTables, manifests, WALs, or
//! snapshot registries.
//!
//! For one strictly increasing key history and floor `F`, safe retention keeps:
//! - every version whose sequence is `>= F`; and
//! - the single newest version `< F`, when one exists, as the baseline visible
//!   to snapshots at/after `F` until a newer version supersedes it.
//!
//! Every obsolete version is therefore strictly older than the first retained
//! barrier version. That barrier is the cross-authority safety property: stale
//! copies of obsolete versions cannot outrank it for any snapshot at/above the
//! floor. If the barrier is a tombstone, the tombstone remains present and keeps
//! stale lower-authority values masked.

use std::fmt;

use super::tablet::{TabletSnapshotRow, VersionedValue};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MvccRetentionPlan {
    retained: Vec<VersionedValue>,
    obsolete: Vec<VersionedValue>,
    barrier_sequence: u64,
}

impl MvccRetentionPlan {
    pub fn retained(&self) -> &[VersionedValue] {
        &self.retained
    }

    pub fn obsolete(&self) -> &[VersionedValue] {
        &self.obsolete
    }

    pub fn barrier_sequence(&self) -> u64 {
        self.barrier_sequence
    }

    pub fn into_retained(self) -> Vec<VersionedValue> {
        self.retained
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MvccGcStats {
    row_count: usize,
    versions_before: usize,
    versions_retained: usize,
    versions_obsolete: usize,
    tombstone_barriers: usize,
}

impl MvccGcStats {
    pub fn row_count(&self) -> usize {
        self.row_count
    }

    pub fn versions_before(&self) -> usize {
        self.versions_before
    }

    pub fn versions_retained(&self) -> usize {
        self.versions_retained
    }

    pub fn versions_obsolete(&self) -> usize {
        self.versions_obsolete
    }

    pub fn tombstone_barriers(&self) -> usize {
        self.tombstone_barriers
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MvccRowRetentionPlan {
    rows: Vec<TabletSnapshotRow>,
    stats: MvccGcStats,
}

impl MvccRowRetentionPlan {
    pub fn rows(&self) -> &[TabletSnapshotRow] {
        &self.rows
    }

    pub fn stats(&self) -> MvccGcStats {
        self.stats
    }

    pub fn into_rows(self) -> Vec<TabletSnapshotRow> {
        self.rows
    }
}

pub fn plan_versions_for_floor(
    versions: &[VersionedValue],
    floor: u64,
) -> Result<MvccRetentionPlan, MvccRetentionError> {
    validate_history(versions)?;

    let first_at_or_above = versions.partition_point(|version| version.sequence < floor);
    let retained_start = first_at_or_above.saturating_sub(1);
    let barrier_sequence = versions[retained_start].sequence;

    Ok(MvccRetentionPlan {
        retained: versions[retained_start..].to_vec(),
        obsolete: versions[..retained_start].to_vec(),
        barrier_sequence,
    })
}

pub fn retain_versions_for_floor(
    versions: &[VersionedValue],
    floor: u64,
) -> Result<Vec<VersionedValue>, MvccRetentionError> {
    Ok(plan_versions_for_floor(versions, floor)?.into_retained())
}

pub fn plan_rows_for_floor(
    rows: &[TabletSnapshotRow],
    floor: u64,
) -> Result<MvccRowRetentionPlan, MvccRetentionError> {
    let mut retained_rows = Vec::with_capacity(rows.len());
    let mut stats = MvccGcStats {
        row_count: rows.len(),
        ..MvccGcStats::default()
    };

    for row in rows {
        let plan = plan_versions_for_floor(&row.versions, floor)?;
        stats.versions_before += row.versions.len();
        stats.versions_retained += plan.retained().len();
        stats.versions_obsolete += plan.obsolete().len();
        if plan
            .retained()
            .first()
            .is_some_and(|version| version.value.is_none())
        {
            stats.tombstone_barriers += 1;
        }
        retained_rows.push(TabletSnapshotRow {
            key: row.key.clone(),
            versions: plan.into_retained(),
        });
    }

    debug_assert_eq!(
        stats.versions_before,
        stats.versions_retained + stats.versions_obsolete
    );

    Ok(MvccRowRetentionPlan {
        rows: retained_rows,
        stats,
    })
}

fn validate_history(versions: &[VersionedValue]) -> Result<(), MvccRetentionError> {
    let Some(first) = versions.first() else {
        return Err(MvccRetentionError::EmptyHistory);
    };
    if first.sequence == 0 {
        return Err(MvccRetentionError::ZeroSequence);
    }

    let mut previous = first.sequence;
    for version in &versions[1..] {
        if version.sequence == 0 {
            return Err(MvccRetentionError::ZeroSequence);
        }
        if version.sequence <= previous {
            return Err(MvccRetentionError::SequencesNotStrictlyIncreasing);
        }
        previous = version.sequence;
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MvccRetentionError {
    EmptyHistory,
    ZeroSequence,
    SequencesNotStrictlyIncreasing,
}

impl fmt::Display for MvccRetentionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyHistory => f.write_str("MVCC retention history is empty"),
            Self::ZeroSequence => f.write_str("MVCC retention history contains sequence zero"),
            Self::SequencesNotStrictlyIncreasing => {
                f.write_str("MVCC retention history is not strictly increasing")
            }
        }
    }
}

impl std::error::Error for MvccRetentionError {}
