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
//! Tombstones follow the same rule and are never removed specially here. Dropping
//! a baseline tombstone requires proof that no lower durable authority can still
//! contain a masked value, which belongs to a later compaction-level policy.

use std::fmt;

use super::tablet::VersionedValue;

pub fn retain_versions_for_floor(
    versions: &[VersionedValue],
    floor: u64,
) -> Result<Vec<VersionedValue>, MvccRetentionError> {
    validate_history(versions)?;

    let first_at_or_above = versions.partition_point(|version| version.sequence < floor);
    let retained_start = first_at_or_above.saturating_sub(1);
    Ok(versions[retained_start..].to_vec())
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
