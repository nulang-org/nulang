//! Authoritative logical-actor ownership state.
//!
//! This module is deliberately a deterministic state machine, not a consensus
//! implementation. A Nulang Cloud control plane can persist/replicate these
//! transitions through Raft, a transactional database, or another linearizable
//! mechanism, while the runtime consumes the resulting grants as fencing input.

use std::collections::HashMap;
use std::fmt;

use super::{ActivationEpoch, ActivationHandle, GrainId, LogicalActorId, NodeId};

/// One currently active ownership grant for a logical actor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogicalActorOwnershipRecord {
    pub grain_id: GrainId,
    pub logical_id: LogicalActorId,
    pub node_id: NodeId,
    pub activation_handle: ActivationHandle,
    pub epoch: ActivationEpoch,
}

#[derive(Debug, Clone)]
struct OwnershipEntry {
    logical_id: LogicalActorId,
    last_epoch: ActivationEpoch,
    owner: Option<(NodeId, ActivationHandle)>,
}

/// Invalid authoritative ownership transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogicalActorOwnershipError {
    /// The attempted operation carries an epoch older than one already seen.
    StaleEpoch {
        known: ActivationEpoch,
        attempted: ActivationEpoch,
    },
    /// Two different live owners are claiming the same authoritative epoch.
    ///
    /// Local arrival order must never resolve this condition. The external
    /// ownership service must choose an owner and advance/fence the epoch.
    EqualEpochConflict {
        epoch: ActivationEpoch,
        existing_node: NodeId,
        existing_handle: ActivationHandle,
        attempted_node: NodeId,
        attempted_handle: ActivationHandle,
    },
    /// This epoch has already been explicitly fenced/released. Re-activation
    /// requires a strictly newer epoch.
    EpochFenced { epoch: ActivationEpoch },
}

impl fmt::Display for LogicalActorOwnershipError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LogicalActorOwnershipError::StaleEpoch { known, attempted } => write!(
                f,
                "stale logical-actor ownership epoch {} (known {})",
                attempted.get(),
                known.get()
            ),
            LogicalActorOwnershipError::EqualEpochConflict {
                epoch,
                existing_node,
                existing_handle,
                attempted_node,
                attempted_handle,
            } => write!(
                f,
                "conflicting logical-actor owners at epoch {}: {:?}/{} vs {:?}/{}",
                epoch.get(),
                existing_node,
                existing_handle.get(),
                attempted_node,
                attempted_handle.get()
            ),
            LogicalActorOwnershipError::EpochFenced { epoch } => write!(
                f,
                "logical-actor ownership epoch {} is already fenced",
                epoch.get()
            ),
        }
    }
}

impl std::error::Error for LogicalActorOwnershipError {}

/// Deterministic authoritative ownership table keyed by the full logical actor
/// identity.
///
/// The compact 128-bit `LogicalActorId` is stored with each entry for
/// persistence/wire indexing, but the full `GrainId` remains the map key so a
/// digest collision cannot silently alias two actors.
#[derive(Debug, Default)]
pub struct LogicalActorOwnershipDirectory {
    entries: HashMap<GrainId, OwnershipEntry>,
}

impl LogicalActorOwnershipDirectory {
    pub fn new() -> Self {
        Self::default()
    }

    /// Install an authoritative ownership grant.
    ///
    /// A strictly newer epoch replaces any prior owner. Replaying the exact
    /// same grant is idempotent. A same-epoch different owner is an explicit
    /// conflict, while a grant for an already-fenced epoch is rejected.
    pub fn grant(
        &mut self,
        grain_id: GrainId,
        node_id: NodeId,
        activation_handle: ActivationHandle,
        epoch: ActivationEpoch,
    ) -> Result<LogicalActorOwnershipRecord, LogicalActorOwnershipError> {
        match self.entries.get_mut(&grain_id) {
            Some(entry) if epoch < entry.last_epoch => {
                return Err(LogicalActorOwnershipError::StaleEpoch {
                    known: entry.last_epoch,
                    attempted: epoch,
                });
            }
            Some(entry) if epoch == entry.last_epoch => {
                match entry.owner {
                    Some((existing_node, existing_handle))
                        if existing_node == node_id && existing_handle == activation_handle =>
                    {
                        return Ok(record(
                            grain_id,
                            entry.logical_id,
                            node_id,
                            activation_handle,
                            epoch,
                        ));
                    }
                    Some((existing_node, existing_handle)) => {
                        return Err(LogicalActorOwnershipError::EqualEpochConflict {
                            epoch,
                            existing_node,
                            existing_handle,
                            attempted_node: node_id,
                            attempted_handle: activation_handle,
                        });
                    }
                    None => {
                        return Err(LogicalActorOwnershipError::EpochFenced { epoch });
                    }
                }
            }
            Some(entry) => {
                entry.last_epoch = epoch;
                entry.owner = Some((node_id, activation_handle));
                return Ok(record(
                    grain_id,
                    entry.logical_id,
                    node_id,
                    activation_handle,
                    epoch,
                ));
            }
            None => {}
        }

        let logical_id = grain_id.logical_id();
        self.entries.insert(
            grain_id.clone(),
            OwnershipEntry {
                logical_id,
                last_epoch: epoch,
                owner: Some((node_id, activation_handle)),
            },
        );
        Ok(record(
            grain_id,
            logical_id,
            node_id,
            activation_handle,
            epoch,
        ))
    }

    /// Fence/release ownership at `epoch`.
    ///
    /// A higher epoch can be installed directly as a tombstone, allowing an
    /// authoritative control plane to invalidate all older owners even when no
    /// replacement node is ready yet. Fencing the current epoch is idempotent.
    pub fn fence(
        &mut self,
        grain_id: GrainId,
        epoch: ActivationEpoch,
    ) -> Result<Option<LogicalActorOwnershipRecord>, LogicalActorOwnershipError> {
        let logical_id = grain_id.logical_id();
        let Some(entry) = self.entries.get_mut(&grain_id) else {
            self.entries.insert(
                grain_id,
                OwnershipEntry {
                    logical_id,
                    last_epoch: epoch,
                    owner: None,
                },
            );
            return Ok(None);
        };

        if epoch < entry.last_epoch {
            return Err(LogicalActorOwnershipError::StaleEpoch {
                known: entry.last_epoch,
                attempted: epoch,
            });
        }

        let previous = entry.owner.map(|(node_id, activation_handle)| {
            record(
                grain_id.clone(),
                entry.logical_id,
                node_id,
                activation_handle,
                entry.last_epoch,
            )
        });
        entry.last_epoch = epoch;
        entry.owner = None;
        Ok(previous)
    }

    /// Return the current active ownership record, if the logical actor is not
    /// presently fenced.
    pub fn record_for(&self, grain_id: &GrainId) -> Option<LogicalActorOwnershipRecord> {
        let entry = self.entries.get(grain_id)?;
        let (node_id, activation_handle) = entry.owner?;
        Some(record(
            grain_id.clone(),
            entry.logical_id,
            node_id,
            activation_handle,
            entry.last_epoch,
        ))
    }

    /// Highest authoritative epoch observed, including fenced/tombstoned
    /// epochs that currently have no owner.
    pub fn last_epoch_for(&self, grain_id: &GrainId) -> Option<ActivationEpoch> {
        self.entries.get(grain_id).map(|entry| entry.last_epoch)
    }

    /// Storage-level fencing predicate: durable writes from an owner are valid
    /// only when both node and epoch match the current authoritative grant.
    ///
    /// Activation handles are intentionally excluded because they are local
    /// ephemeral routing metadata and must not become a durable storage key.
    pub fn authorizes_commit(
        &self,
        grain_id: &GrainId,
        node_id: NodeId,
        epoch: ActivationEpoch,
    ) -> bool {
        self.entries.get(grain_id).is_some_and(|entry| {
            entry.last_epoch == epoch
                && entry
                    .owner
                    .is_some_and(|(owner_node, _)| owner_node == node_id)
        })
    }

    /// Routing-level fencing predicate including the runtime-local activation
    /// handle.
    pub fn is_current_route(
        &self,
        grain_id: &GrainId,
        node_id: NodeId,
        activation_handle: ActivationHandle,
        epoch: ActivationEpoch,
    ) -> bool {
        self.entries.get(grain_id).is_some_and(|entry| {
            entry.last_epoch == epoch
                && entry.owner == Some((node_id, activation_handle))
        })
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

fn record(
    grain_id: GrainId,
    logical_id: LogicalActorId,
    node_id: NodeId,
    activation_handle: ActivationHandle,
    epoch: ActivationEpoch,
) -> LogicalActorOwnershipRecord {
    LogicalActorOwnershipRecord {
        grain_id,
        logical_id,
        node_id,
        activation_handle,
        epoch,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handle(raw: u64) -> ActivationHandle {
        ActivationHandle::new(raw).unwrap()
    }

    fn epoch(raw: u64) -> ActivationEpoch {
        ActivationEpoch::new(raw).unwrap()
    }

    #[test]
    fn exact_grant_replay_is_idempotent() {
        let mut directory = LogicalActorOwnershipDirectory::new();
        let grain = GrainId::new("Cart", "customer-42");

        let first = directory
            .grant(grain.clone(), NodeId(1), handle(10), epoch(3))
            .unwrap();
        let replay = directory
            .grant(grain.clone(), NodeId(1), handle(10), epoch(3))
            .unwrap();

        assert_eq!(first, replay);
        assert_eq!(directory.record_for(&grain), Some(first));
    }

    #[test]
    fn stale_epoch_is_rejected() {
        let mut directory = LogicalActorOwnershipDirectory::new();
        let grain = GrainId::new("Cart", "stale");
        directory
            .grant(grain.clone(), NodeId(1), handle(10), epoch(5))
            .unwrap();

        assert_eq!(
            directory.grant(grain, NodeId(2), handle(20), epoch(4)),
            Err(LogicalActorOwnershipError::StaleEpoch {
                known: epoch(5),
                attempted: epoch(4),
            })
        );
    }

    #[test]
    fn equal_epoch_different_owner_is_an_explicit_conflict() {
        let mut directory = LogicalActorOwnershipDirectory::new();
        let grain = GrainId::new("Cart", "split-brain");
        directory
            .grant(grain.clone(), NodeId(1), handle(10), epoch(7))
            .unwrap();

        assert_eq!(
            directory.grant(grain, NodeId(2), handle(20), epoch(7)),
            Err(LogicalActorOwnershipError::EqualEpochConflict {
                epoch: epoch(7),
                existing_node: NodeId(1),
                existing_handle: handle(10),
                attempted_node: NodeId(2),
                attempted_handle: handle(20),
            })
        );
    }

    #[test]
    fn fence_blocks_same_epoch_resurrection_but_allows_newer_grant() {
        let mut directory = LogicalActorOwnershipDirectory::new();
        let grain = GrainId::new("Cart", "handoff");
        let original = directory
            .grant(grain.clone(), NodeId(1), handle(10), epoch(8))
            .unwrap();

        assert_eq!(
            directory.fence(grain.clone(), epoch(8)).unwrap(),
            Some(original)
        );
        assert!(directory.record_for(&grain).is_none());
        assert_eq!(directory.last_epoch_for(&grain), Some(epoch(8)));
        assert_eq!(
            directory.grant(grain.clone(), NodeId(1), handle(11), epoch(8)),
            Err(LogicalActorOwnershipError::EpochFenced { epoch: epoch(8) })
        );

        let replacement = directory
            .grant(grain.clone(), NodeId(2), handle(12), epoch(9))
            .unwrap();
        assert_eq!(replacement.node_id, NodeId(2));
        assert_eq!(replacement.epoch, epoch(9));
    }

    #[test]
    fn commit_and_route_fences_have_different_handle_requirements() {
        let mut directory = LogicalActorOwnershipDirectory::new();
        let grain = GrainId::new("Cart", "commit-fence");
        directory
            .grant(grain.clone(), NodeId(4), handle(44), epoch(11))
            .unwrap();

        assert!(directory.authorizes_commit(&grain, NodeId(4), epoch(11)));
        assert!(!directory.authorizes_commit(&grain, NodeId(5), epoch(11)));
        assert!(!directory.authorizes_commit(&grain, NodeId(4), epoch(10)));

        assert!(directory.is_current_route(&grain, NodeId(4), handle(44), epoch(11)));
        assert!(!directory.is_current_route(&grain, NodeId(4), handle(45), epoch(11)));

        directory.fence(grain.clone(), epoch(12)).unwrap();
        assert!(!directory.authorizes_commit(&grain, NodeId(4), epoch(11)));
        assert!(!directory.is_current_route(&grain, NodeId(4), handle(44), epoch(11)));
    }
}
