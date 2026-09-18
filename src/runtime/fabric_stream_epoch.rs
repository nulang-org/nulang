//! Quorum-backed Fabric Stream epoch transitions.
//!
//! This module deliberately implements a conservative first transition
//! protocol. A new epoch can be installed only when:
//! - the proposal advances to a strictly higher election term,
//! - the prospective leader is the deterministic leader of the proposed set,
//! - every new replica is drawn from the old replica set,
//! - an old-policy majority durably promises the proposal,
//! - every affirmative voter has the exact same durable tail as the candidate,
//! - every new replica is among those affirmative voters.
//!
//! A durable promise fences the old epoch immediately. This is crash-safe and
//! prevents the old leader from forming a commit quorum after a majority has
//! moved its promise forward. Automatic failover remains a separate layer.

use std::collections::HashSet;
use std::io;

use serde::{Deserialize, Serialize};

use crate::runtime::fabric_stream::{
    FabricStreamEpochProposalState, FabricStreamEpochTransitionState, FabricStreamEpochVoteState,
    FabricStreamReplicationPolicy,
};
use crate::runtime::fabric_stream_cluster::{
    compute_stream_placement, compute_stream_placement_from_candidates, FabricStreamPlacement,
};
use crate::runtime::{ClusterState, MessagePriority, NodeId, NodeStatus, Packet, Runtime};

pub(crate) const FABRIC_STREAM_EPOCH_PREPARE_BEHAVIOR: &str =
    "__nulang_fabric_stream_epoch_prepare_v1";
pub(crate) const FABRIC_STREAM_EPOCH_VOTE_BEHAVIOR: &str = "__nulang_fabric_stream_epoch_vote_v1";
pub(crate) const FABRIC_STREAM_EPOCH_COMMIT_BEHAVIOR: &str =
    "__nulang_fabric_stream_epoch_commit_v1";
pub(crate) const FABRIC_STREAM_EPOCH_REPAIR_BEHAVIOR: &str =
    "__nulang_fabric_stream_epoch_repair_v1";
pub(crate) const FABRIC_STREAM_EPOCH_PULL_REQUEST_BEHAVIOR: &str =
    "__nulang_fabric_stream_epoch_pull_request_v1";
pub(crate) const FABRIC_STREAM_EPOCH_PULL_RESPONSE_BEHAVIOR: &str =
    "__nulang_fabric_stream_epoch_pull_response_v1";

const FABRIC_STREAM_FAILOVER_RETRY_INITIAL: std::time::Duration =
    std::time::Duration::from_millis(500);
const FABRIC_STREAM_FAILOVER_RETRY_MAX: std::time::Duration = std::time::Duration::from_secs(30);
const FABRIC_STREAM_AUTO_REPAIR_MAX_RECORDS: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FabricStreamEpochTransitionStatus {
    pub from_epoch: u64,
    pub to_epoch: u64,
    pub affirmative_votes: usize,
    pub rejected_votes: usize,
    pub quorum: usize,
    pub finalized: bool,
    pub committed_sequence: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FabricStreamAutoFailoverReport {
    pub streams_examined: usize,
    pub leader_streams: usize,
    pub transitions_started: usize,
    pub skipped_no_old_quorum: usize,
    pub skipped_not_candidate: usize,
    pub skipped_requires_new_replica: usize,
    pub errors: usize,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FabricStreamEpochRepairReport {
    pub replicas_examined: usize,
    pub records_dispatched: usize,
    pub unavailable_replicas: usize,
    pub ahead_replicas: usize,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FabricStreamEpochPullReport {
    pub ahead_replicas: usize,
    pub requests_dispatched: usize,
    pub unavailable_replicas: usize,
    pub source_tail: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FabricStreamEpochPullRequest {
    pub stream: String,
    pub proposal: FabricStreamEpochProposalState,
    pub requester: u64,
    pub source: u64,
    pub start_sequence: u64,
    pub max_records: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FabricStreamEpochPullResponse {
    pub stream: String,
    pub proposal: FabricStreamEpochProposalState,
    pub requester: u64,
    pub source: u64,
    pub source_tail: u64,
    pub records: Vec<FabricStreamEpochRepairRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FabricStreamEpochRepairRecord {
    pub sequence: u64,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FabricStreamEpochRepairBatch {
    pub stream: String,
    pub proposal: FabricStreamEpochProposalState,
    pub target: u64,
    pub records: Vec<FabricStreamEpochRepairRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FabricStreamEpochPrepare {
    pub stream: String,
    pub proposal: FabricStreamEpochProposalState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FabricStreamEpochVote {
    pub stream: String,
    pub vote: FabricStreamEpochVoteState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FabricStreamEpochCommit {
    pub stream: String,
    pub proposal: FabricStreamEpochProposalState,
    pub affirmative_voters: Vec<u64>,
    pub committed_sequence: u64,
}

#[derive(Debug, Clone)]
pub(crate) struct FabricStreamEpochVoteOutcome {
    pub status: FabricStreamEpochTransitionStatus,
    pub commit: Option<FabricStreamEpochCommit>,
}

impl FabricStreamEpochPullRequest {
    pub(crate) fn to_wire_bytes(&self) -> io::Result<Vec<u8>> {
        serde_json::to_vec(self).map_err(json_error)
    }

    pub(crate) fn from_wire_bytes(bytes: &[u8]) -> io::Result<Self> {
        serde_json::from_slice(bytes).map_err(json_error)
    }
}

impl FabricStreamEpochPullResponse {
    pub(crate) fn to_wire_bytes(&self) -> io::Result<Vec<u8>> {
        serde_json::to_vec(self).map_err(json_error)
    }

    pub(crate) fn from_wire_bytes(bytes: &[u8]) -> io::Result<Self> {
        serde_json::from_slice(bytes).map_err(json_error)
    }
}

impl FabricStreamEpochRepairBatch {
    pub(crate) fn to_wire_bytes(&self) -> io::Result<Vec<u8>> {
        serde_json::to_vec(self).map_err(json_error)
    }

    pub(crate) fn from_wire_bytes(bytes: &[u8]) -> io::Result<Self> {
        serde_json::from_slice(bytes).map_err(json_error)
    }
}

impl FabricStreamEpochPrepare {
    pub(crate) fn to_wire_bytes(&self) -> io::Result<Vec<u8>> {
        serde_json::to_vec(self).map_err(json_error)
    }

    pub(crate) fn from_wire_bytes(bytes: &[u8]) -> io::Result<Self> {
        serde_json::from_slice(bytes).map_err(json_error)
    }
}

impl FabricStreamEpochVote {
    pub(crate) fn to_wire_bytes(&self) -> io::Result<Vec<u8>> {
        serde_json::to_vec(self).map_err(json_error)
    }

    pub(crate) fn from_wire_bytes(bytes: &[u8]) -> io::Result<Self> {
        serde_json::from_slice(bytes).map_err(json_error)
    }
}

impl FabricStreamEpochCommit {
    pub(crate) fn to_wire_bytes(&self) -> io::Result<Vec<u8>> {
        serde_json::to_vec(self).map_err(json_error)
    }

    pub(crate) fn from_wire_bytes(bytes: &[u8]) -> io::Result<Self> {
        serde_json::from_slice(bytes).map_err(json_error)
    }
}

impl Runtime {
    fn fabric_stream_locally_eligible_old_replicas(
        policy: &FabricStreamReplicationPolicy,
        cluster: &ClusterState,
    ) -> Vec<NodeId> {
        policy
            .replicas
            .iter()
            .copied()
            .map(NodeId)
            .filter(|node| {
                if cluster.is_removed(*node) {
                    return false;
                }
                !matches!(
                    cluster.get_node(*node).map(|info| info.status),
                    Some(NodeStatus::Failed | NodeStatus::Leaving)
                )
            })
            .collect()
    }

    fn fabric_stream_validate_transition_placement(
        &self,
        stream: &str,
        proposal: &FabricStreamEpochProposalState,
        cluster: &ClusterState,
    ) -> io::Result<FabricStreamPlacement> {
        let local = self.distributed.node_id.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                "Fabric epoch transition placement requires distribution",
            )
        })?;

        // Preserve ordinary manual/reconfiguration behavior when the current
        // whole-cluster candidate placement already matches the proposal.
        if let Ok(placement) = compute_stream_placement(
            local,
            Some(cluster),
            stream,
            proposal.to_policy.partition,
            proposal.to_policy.replication_factor,
        ) {
            if policy_from_placement(&placement, proposal.to_policy.epoch)
                == proposal.to_policy
            {
                return Ok(placement);
            }
        }

        // Pre-removal failover may reduce only the installed old replica set.
        // Missing/unknown and Suspicious old replicas remain eligible. A node
        // is excluded only after this receiver independently sees Failed,
        // Leaving, or confirmed Removed state.
        let eligible =
            Self::fabric_stream_locally_eligible_old_replicas(&proposal.from_policy, cluster);
        if proposal.to_policy.replication_factor > eligible.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Fabric epoch proposal requires more replicas than locally eligible old policy",
            ));
        }
        let reduced = compute_stream_placement_from_candidates(
            stream,
            proposal.to_policy.partition,
            proposal.to_policy.replication_factor,
            &eligible,
        )?;
        if policy_from_placement(&reduced, proposal.to_policy.epoch) != proposal.to_policy {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Fabric epoch proposal does not match locally corroborated survivor placement",
            ));
        }
        Ok(reduced)
    }

    /// Start safe stream ownership transitions after a node is confirmed removed.
    ///
    /// This is intentionally narrower than generic reconfiguration:
    /// - only streams whose durable leader is the removed node are considered,
    /// - enough surviving old replicas must remain for the old-policy quorum,
    /// - the reduced current placement must contain only surviving old replicas,
    /// - only the deterministic proposed leader initiates the transition.
    pub fn fabric_stream_failover_confirmed_removed(
        &mut self,
        removed: NodeId,
    ) -> io::Result<FabricStreamAutoFailoverReport> {
        let local = self.distributed.node_id.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                "Fabric automatic failover requires distribution",
            )
        })?;
        if self.distributed.cluster.is_none() {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "Fabric automatic failover requires cluster membership",
            ));
        }

        let streams = match self.fabric_stream_names() {
            Ok(streams) => streams,
            Err(error) if error.kind() == io::ErrorKind::NotConnected => {
                return Ok(FabricStreamAutoFailoverReport::default());
            }
            Err(error) => return Err(error),
        };
        let mut report = FabricStreamAutoFailoverReport::default();

        for stream in streams {
            report.streams_examined += 1;
            let policy = match self.fabric_stream_replication_policy(&stream) {
                Ok(Some(policy)) => policy,
                Ok(None) => continue,
                Err(error) => {
                    report.errors += 1;
                    tracing::warn!(
                        stream = %stream,
                        "nulang-fabric-stream: cannot inspect failover policy: {}",
                        error
                    );
                    continue;
                }
            };
            if policy.leader != removed.0 {
                continue;
            }
            report.leader_streams += 1;

            let surviving: Vec<u64> = {
                let cluster = self
                    .distributed
                    .cluster
                    .as_ref()
                    .expect("cluster availability checked above");
                policy
                    .replicas
                    .iter()
                    .copied()
                    .filter(|node| !cluster.is_removed(NodeId(*node)))
                    .collect()
            };
            let old_quorum = policy.replication_factor / 2 + 1;
            if surviving.len() < old_quorum {
                report.skipped_no_old_quorum += 1;
                tracing::warn!(
                    stream = %stream,
                    old_replication_factor = policy.replication_factor,
                    survivors = surviving.len(),
                    old_quorum,
                    "nulang-fabric-stream: confirmed leader removal cannot fail over without old-policy quorum"
                );
                continue;
            }

            let new_replication_factor = surviving.len();
            let placement = match self.fabric_stream_placement(
                &stream,
                policy.partition,
                new_replication_factor,
            ) {
                Ok(placement) => placement,
                Err(error) => {
                    report.errors += 1;
                    tracing::warn!(
                        stream = %stream,
                        "nulang-fabric-stream: cannot compute failover placement: {}",
                        error
                    );
                    continue;
                }
            };

            let surviving_set: HashSet<u64> = surviving.iter().copied().collect();
            let placement_set: HashSet<u64> =
                placement.replicas.iter().map(|node| node.0).collect();
            if placement_set != surviving_set {
                report.skipped_requires_new_replica += 1;
                tracing::warn!(
                    stream = %stream,
                    "nulang-fabric-stream: automatic failover would require replica-set expansion; skipping"
                );
                continue;
            }
            if placement.leader != local {
                report.skipped_not_candidate += 1;
                continue;
            }

            match self.fabric_stream_begin_epoch_transition(
                &stream,
                policy.partition,
                new_replication_factor,
            ) {
                Ok(status) => {
                    report.transitions_started += 1;
                    if !status.finalized {
                        let retry_at = self.now() + FABRIC_STREAM_FAILOVER_RETRY_INITIAL;
                        self.distributed.fabric_stream_failover_retry.insert(
                            stream.clone(),
                            (retry_at, FABRIC_STREAM_FAILOVER_RETRY_INITIAL),
                        );
                    }
                    tracing::info!(
                        stream = %stream,
                        removed = removed.0,
                        from_epoch = status.from_epoch,
                        to_epoch = status.to_epoch,
                        new_replication_factor,
                        "nulang-fabric-stream: started automatic confirmed-removal failover"
                    );
                }
                Err(error) => {
                    report.errors += 1;
                    tracing::warn!(
                        stream = %stream,
                        "nulang-fabric-stream: automatic failover transition failed: {}",
                        error
                    );
                }
            }
        }

        Ok(report)
    }

    fn fabric_stream_drive_auto_failover_once(
        &mut self,
        stream: &str,
    ) -> io::Result<FabricStreamEpochTransitionStatus> {
        let state = self
            .fabric_stream_epoch_transition_state(stream)?
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "Fabric automatic failover transition is not in progress",
                )
            })?;

        if state.finalized {
            return self.fabric_stream_resume_epoch_transition(stream);
        }

        let local = self.distributed.node_id.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                "Fabric automatic failover requires distribution",
            )
        })?;
        if state.proposal.to_policy.leader != local.0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "local node is not the Fabric automatic-failover candidate",
            ));
        }

        let local_tail = self.fabric_stream_info(stream)?.last_sequence.unwrap_or(0);
        if local_tail != state.proposal.candidate_tail {
            tracing::info!(
                stream = %stream,
                from_term = state.proposal.to_policy.epoch,
                old_candidate_tail = state.proposal.candidate_tail,
                new_candidate_tail = local_tail,
                "nulang-fabric-stream: automatic failover candidate tail changed; superseding with higher term"
            );
            return self.fabric_stream_begin_epoch_transition(
                stream,
                state.proposal.to_policy.partition,
                state.proposal.to_policy.replication_factor,
            );
        }

        let mut has_ahead = false;
        let mut has_behind = false;
        for vote in state.votes.values() {
            if vote.accepted || !state.proposal.to_policy.replicas.contains(&vote.voter) {
                continue;
            }
            if vote.tail > state.proposal.candidate_tail {
                has_ahead = true;
            } else if vote.tail < state.proposal.candidate_tail {
                has_behind = true;
            }
        }

        if has_ahead {
            let report = self.fabric_stream_pull_epoch_transition(
                stream,
                FABRIC_STREAM_AUTO_REPAIR_MAX_RECORDS,
            )?;
            if report.requests_dispatched > 0 {
                tracing::info!(
                    stream = %stream,
                    source_tail = report.source_tail,
                    ahead_replicas = report.ahead_replicas,
                    "nulang-fabric-stream: automatic failover requested candidate reconciliation from ahead survivor"
                );
                return Ok(status_from_state(&state));
            }
        }

        if has_behind {
            let report = self.fabric_stream_repair_epoch_transition(
                stream,
                FABRIC_STREAM_AUTO_REPAIR_MAX_RECORDS,
            )?;
            if report.records_dispatched > 0 {
                tracing::info!(
                    stream = %stream,
                    records = report.records_dispatched,
                    replicas = report.replicas_examined,
                    "nulang-fabric-stream: automatic failover pushed proposal-scoped repair to lagging survivor"
                );
                return Ok(status_from_state(&state));
            }
        }

        self.fabric_stream_resume_epoch_transition(stream)
    }

    /// Retry active automatic ownership transitions with logical-clock backoff.
    ///
    /// Durable transition state is the restart source of truth; if the
    /// in-memory schedule is absent after restart, a local candidate rebuilds
    /// an immediate retry entry from the transition file.
    pub(crate) fn fabric_stream_tick_auto_failover(&mut self) {
        let now = self.now();
        let local = match self.distributed.node_id {
            Some(local) => local,
            None => return,
        };

        let streams = match self.fabric_stream_names() {
            Ok(streams) => streams,
            Err(_) => return,
        };
        for stream in streams {
            let state = match self.fabric_stream_epoch_transition_state(&stream) {
                Ok(Some(state)) => state,
                _ => continue,
            };
            if state.finalized || state.proposal.to_policy.leader != local.0 {
                self.distributed
                    .fabric_stream_failover_retry
                    .remove(&stream);
                continue;
            }
            self.distributed
                .fabric_stream_failover_retry
                .entry(stream)
                .or_insert((now, FABRIC_STREAM_FAILOVER_RETRY_INITIAL));
        }

        let due: Vec<String> = self
            .distributed
            .fabric_stream_failover_retry
            .iter()
            .filter(|(_, (next_attempt, _))| *next_attempt <= now)
            .map(|(stream, _)| stream.clone())
            .collect();

        for stream in due {
            let result = self.fabric_stream_drive_auto_failover_once(&stream);
            match result {
                Ok(status) if status.finalized => {
                    self.distributed
                        .fabric_stream_failover_retry
                        .remove(&stream);
                }
                Ok(_) | Err(_) => {
                    let now = self.now();
                    if let Some((next_attempt, backoff)) = self
                        .distributed
                        .fabric_stream_failover_retry
                        .get_mut(&stream)
                    {
                        *backoff = backoff
                            .saturating_mul(2)
                            .min(FABRIC_STREAM_FAILOVER_RETRY_MAX);
                        *next_attempt = now + *backoff;
                    }
                    if let Err(error) = result {
                        tracing::warn!(
                            stream = %stream,
                            "nulang-fabric-stream: automatic failover retry failed: {}",
                            error
                        );
                    }
                }
            }
        }
    }

    /// Start or resume a quorum-backed transition to a higher election term.
    ///
    /// The proposed replica set is the current deterministic placement for
    /// `new_replication_factor`. For this first protocol version every new
    /// replica must already belong to the old replica set.
    pub fn fabric_stream_begin_epoch_transition(
        &mut self,
        stream: &str,
        partition: u16,
        new_replication_factor: usize,
    ) -> io::Result<FabricStreamEpochTransitionStatus> {
        let from_policy = self
            .fabric_stream_replication_policy(stream)?
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "Fabric stream replication policy is not established",
                )
            })?;
        if from_policy.partition != partition {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Fabric epoch transition partition differs from durable policy",
            ));
        }

        let placement = self.fabric_stream_placement(stream, partition, new_replication_factor)?;
        let local = self.distributed.node_id.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                "Fabric epoch transition requires distribution",
            )
        })?;
        if placement.leader != local {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                format!(
                    "prospective Fabric epoch leader is {:?}, local node is {:?}",
                    placement.leader, local
                ),
            ));
        }
        if !from_policy.replicas.contains(&local.0) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "prospective Fabric leader is not a replica in the old policy",
            ));
        }
        if !placement
            .replicas
            .iter()
            .all(|node| from_policy.replicas.contains(&node.0))
        {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "Fabric epoch transition cannot introduce a new replica yet; shrink or transfer within the old replica set",
            ));
        }

        let highest_seen = self
            .fabric_stream_epoch_promise(stream)?
            .map(|promise| promise.epoch)
            .unwrap_or(from_policy.epoch)
            .max(from_policy.epoch);
        let to_epoch = highest_seen
            .checked_add(1)
            .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "Fabric stream epoch overflow"))?;
        let to_policy = policy_from_placement(&placement, to_epoch);
        let candidate_tail = self.fabric_stream_info(stream)?.last_sequence.unwrap_or(0);
        let proposal_hash = epoch_proposal_hash(stream, &from_policy, &to_policy, candidate_tail);
        let proposal = FabricStreamEpochProposalState {
            proposal_hash,
            from_policy,
            to_policy,
            candidate_tail,
        };

        let state = self.fabric_stream_begin_epoch_transition_state(stream, proposal.clone())?;
        if state.finalized {
            self.fabric_stream_complete_finalized_epoch_transition(stream, &state)?;
            self.fabric_stream_dispatch_epoch_commit(&commit_from_state(stream, &state)?)?;
            return Ok(status_from_state(&state));
        }

        // The candidate is an old-policy replica and therefore casts/persists
        // its own promise before asking peers to move forward.
        let cluster = self.distributed.cluster.take().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                "Fabric epoch transition requires cluster membership",
            )
        })?;
        let own_vote =
            self.fabric_stream_evaluate_epoch_prepare(stream, &proposal, local, &cluster);
        self.distributed.cluster = Some(cluster);
        let own_vote = own_vote?;
        let outcome = self.fabric_stream_record_epoch_vote_from_cluster(
            FabricStreamEpochVote {
                stream: stream.to_string(),
                vote: own_vote,
            },
            local,
        )?;

        if let Some(commit) = outcome.commit {
            self.fabric_stream_dispatch_epoch_commit(&commit)?;
            return Ok(outcome.status);
        }

        self.fabric_stream_dispatch_epoch_prepare(FabricStreamEpochPrepare {
            stream: stream.to_string(),
            proposal,
        })?;
        Ok(outcome.status)
    }

    /// Re-send prepare or commit traffic for a durable transition after a
    /// candidate restart. The same proposal hash is reused.
    pub fn fabric_stream_resume_epoch_transition(
        &mut self,
        stream: &str,
    ) -> io::Result<FabricStreamEpochTransitionStatus> {
        let state = self
            .fabric_stream_epoch_transition_state(stream)?
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "Fabric epoch transition is not in progress",
                )
            })?;
        if state.finalized {
            self.fabric_stream_complete_finalized_epoch_transition(stream, &state)?;
            let commit = commit_from_state(stream, &state)?;
            self.fabric_stream_dispatch_epoch_commit(&commit)?;
            return Ok(status_from_state(&state));
        }

        let local = self.distributed.node_id.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                "Fabric epoch transition requires distribution",
            )
        })?;
        let cluster = self.distributed.cluster.take().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                "Fabric epoch transition requires cluster membership",
            )
        })?;
        let own_vote =
            self.fabric_stream_evaluate_epoch_prepare(stream, &state.proposal, local, &cluster);
        self.distributed.cluster = Some(cluster);
        let own_vote = own_vote?;
        let outcome = self.fabric_stream_record_epoch_vote_from_cluster(
            FabricStreamEpochVote {
                stream: stream.to_string(),
                vote: own_vote,
            },
            local,
        )?;
        if let Some(commit) = outcome.commit {
            self.fabric_stream_dispatch_epoch_commit(&commit)?;
            return Ok(outcome.status);
        }

        self.fabric_stream_dispatch_epoch_prepare(FabricStreamEpochPrepare {
            stream: stream.to_string(),
            proposal: state.proposal,
        })?;
        Ok(outcome.status)
    }

    /// Ask one ahead proposed replica for a bounded exact suffix.
    ///
    /// Applying a response changes the candidate tail and therefore invalidates
    /// the current proposal. The response handler immediately starts a strictly
    /// higher election term whose hash binds the new tail.
    pub fn fabric_stream_pull_epoch_transition(
        &mut self,
        stream: &str,
        max_records: usize,
    ) -> io::Result<FabricStreamEpochPullReport> {
        if max_records == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Fabric epoch pull bound must be greater than zero",
            ));
        }
        let state = self
            .fabric_stream_epoch_transition_state(stream)?
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "Fabric epoch transition is not in progress",
                )
            })?;
        if state.finalized {
            return Ok(FabricStreamEpochPullReport::default());
        }

        let local = self.distributed.node_id.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                "Fabric epoch pull requires distribution",
            )
        })?;
        if state.proposal.to_policy.leader != local.0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "only the proposed Fabric leader may pull transition data",
            ));
        }

        let local_tail = self.fabric_stream_info(stream)?.last_sequence.unwrap_or(0);
        if local_tail != state.proposal.candidate_tail {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "Fabric candidate tail already changed; begin a higher term first",
            ));
        }

        let mut ahead: Vec<(u64, u64)> = state
            .votes
            .values()
            .filter(|vote| {
                !vote.accepted
                    && vote.tail > local_tail
                    && state.proposal.to_policy.replicas.contains(&vote.voter)
            })
            .map(|vote| (vote.voter, vote.tail))
            .collect();
        ahead.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));

        let mut report = FabricStreamEpochPullReport {
            ahead_replicas: ahead.len(),
            ..FabricStreamEpochPullReport::default()
        };
        let Some((source_raw, source_tail)) = ahead.first().copied() else {
            return Ok(report);
        };
        report.source_tail = source_tail;
        let source = NodeId(source_raw);

        let cluster = self.distributed.cluster.as_ref().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                "Fabric epoch pull requires cluster membership",
            )
        })?;
        if cluster.is_removed(source) {
            report.unavailable_replicas = 1;
            return Ok(report);
        }
        let Some(address) = cluster
            .get_node(source)
            .filter(|info| matches!(info.status, NodeStatus::Healthy | NodeStatus::Joining))
            .map(|info| info.address)
        else {
            report.unavailable_replicas = 1;
            return Ok(report);
        };

        let request = FabricStreamEpochPullRequest {
            stream: stream.to_string(),
            proposal: state.proposal,
            requester: local.0,
            source: source.0,
            start_sequence: local_tail.saturating_add(1),
            max_records,
        };
        let bytes = request.to_wire_bytes()?;
        let transport = self.distributed.transport.as_mut().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                "Fabric epoch pull requires network transport",
            )
        })?;
        transport.send(
            source,
            address,
            system_packet(FABRIC_STREAM_EPOCH_PULL_REQUEST_BEHAVIOR, local, bytes),
        );
        report.requests_dispatched = 1;
        Ok(report)
    }

    pub(crate) fn fabric_stream_build_epoch_pull_response(
        &mut self,
        request: &FabricStreamEpochPullRequest,
        sender: NodeId,
        cluster: &ClusterState,
    ) -> io::Result<FabricStreamEpochPullResponse> {
        validate_proposal_shape(&request.stream, &request.proposal)?;
        let local = self.distributed.node_id.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                "Fabric epoch pull response requires distribution",
            )
        })?;
        if request.requester != sender.0
            || request.requester != request.proposal.to_policy.leader
            || request.source != local.0
            || !request.proposal.to_policy.replicas.contains(&local.0)
            || request.max_records == 0
            || request.start_sequence != request.proposal.candidate_tail.saturating_add(1)
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "unauthorized Fabric epoch pull request",
            ));
        }

        let current = self
            .fabric_stream_replication_policy(&request.stream)?
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "Fabric stream replication policy is not established",
                )
            })?;
        if current != request.proposal.from_policy {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Fabric epoch pull source policy differs from local durable policy",
            ));
        }
        let placement = compute_stream_placement(
            local,
            Some(cluster),
            &request.stream,
            request.proposal.to_policy.partition,
            request.proposal.to_policy.replication_factor,
        )?;
        if policy_from_placement(&placement, request.proposal.to_policy.epoch)
            != request.proposal.to_policy
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Fabric epoch pull no longer matches current placement",
            ));
        }

        if let Some(promise) = self.fabric_stream_epoch_promise(&request.stream)? {
            if promise.epoch > request.proposal.to_policy.epoch
                || (promise.epoch == request.proposal.to_policy.epoch
                    && promise.proposal_hash != request.proposal.proposal_hash)
            {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "Fabric epoch pull request is fenced by a newer or conflicting promise",
                ));
            }
        }

        let source_tail = self
            .fabric_stream_info(&request.stream)?
            .last_sequence
            .unwrap_or(0);
        if request.start_sequence > source_tail {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Fabric epoch pull starts beyond source durable tail",
            ));
        }

        let records = self
            .fabric_stream_read(&request.stream, request.start_sequence, request.max_records)?
            .into_iter()
            .map(|record| FabricStreamEpochRepairRecord {
                sequence: record.sequence,
                payload: record.payload,
            })
            .collect();
        Ok(FabricStreamEpochPullResponse {
            stream: request.stream.clone(),
            proposal: request.proposal.clone(),
            requester: request.requester,
            source: local.0,
            source_tail,
            records,
        })
    }

    /// Apply a pull response to the candidate and return the replication factor
    /// for the higher-term proposal that must immediately replace it.
    pub(crate) fn fabric_stream_apply_epoch_pull_response(
        &mut self,
        response: &FabricStreamEpochPullResponse,
        sender: NodeId,
        cluster: &ClusterState,
    ) -> io::Result<usize> {
        validate_proposal_shape(&response.stream, &response.proposal)?;
        let local = self.distributed.node_id.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                "Fabric epoch pull apply requires distribution",
            )
        })?;
        if response.requester != local.0
            || response.requester != response.proposal.to_policy.leader
            || response.source != sender.0
            || !response.proposal.to_policy.replicas.contains(&sender.0)
            || response.records.is_empty()
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "unauthorized Fabric epoch pull response",
            ));
        }

        let state = self
            .fabric_stream_epoch_transition_state(&response.stream)?
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "Fabric epoch transition is not in progress",
                )
            })?;
        if state.finalized || state.proposal != response.proposal {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Fabric epoch pull response does not match the active proposal",
            ));
        }

        let current = self
            .fabric_stream_replication_policy(&response.stream)?
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "Fabric stream replication policy is not established",
                )
            })?;
        if current != response.proposal.from_policy {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Fabric epoch pull candidate policy changed",
            ));
        }
        let placement = compute_stream_placement(
            local,
            Some(cluster),
            &response.stream,
            response.proposal.to_policy.partition,
            response.proposal.to_policy.replication_factor,
        )?;
        if policy_from_placement(&placement, response.proposal.to_policy.epoch)
            != response.proposal.to_policy
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Fabric epoch pull response no longer matches current placement",
            ));
        }

        let local_tail = self
            .fabric_stream_info(&response.stream)?
            .last_sequence
            .unwrap_or(0);
        if local_tail != response.proposal.candidate_tail
            || response.records[0].sequence != local_tail.saturating_add(1)
            || response
                .records
                .last()
                .map(|record| record.sequence > response.source_tail)
                .unwrap_or(true)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Fabric epoch pull response is not the next candidate suffix",
            ));
        }

        let source_vote = state.votes.get(&response.source).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "Fabric epoch pull source has no recorded transition vote",
            )
        })?;
        if source_vote.tail != response.source_tail
            || source_vote.tail <= response.proposal.candidate_tail
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Fabric epoch pull source tail differs from its recorded vote",
            ));
        }

        let mut expected = local_tail.saturating_add(1);
        for record in &response.records {
            if record.sequence != expected {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Fabric epoch pull response contains a sequence gap",
                ));
            }
            self.fabric_stream_apply_transition_repair_record(
                &response.stream,
                record.sequence,
                &record.payload,
            )?;
            expected = expected.saturating_add(1);
        }

        Ok(response.proposal.to_policy.replication_factor)
    }

    /// Push a bounded set of proposal-scoped records to rejected new-policy
    /// replicas that are behind the prospective leader.
    ///
    /// Replicas ahead of the candidate are reported but never modified; they
    /// require a future pull/reconciliation layer and a superseding term.
    pub fn fabric_stream_repair_epoch_transition(
        &mut self,
        stream: &str,
        max_records_per_replica: usize,
    ) -> io::Result<FabricStreamEpochRepairReport> {
        if max_records_per_replica == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Fabric epoch repair bound must be greater than zero",
            ));
        }

        let state = self
            .fabric_stream_epoch_transition_state(stream)?
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "Fabric epoch transition is not in progress",
                )
            })?;
        if state.finalized {
            return Ok(FabricStreamEpochRepairReport::default());
        }

        let local = self.distributed.node_id.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                "Fabric epoch repair requires distribution",
            )
        })?;
        if state.proposal.to_policy.leader != local.0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "only the proposed Fabric leader may repair transition voters",
            ));
        }

        let current_tail = self.fabric_stream_info(stream)?.last_sequence.unwrap_or(0);
        if current_tail != state.proposal.candidate_tail {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "Fabric candidate tail changed; start a higher-term proposal before repair",
            ));
        }

        let mut report = FabricStreamEpochRepairReport::default();
        let mut batches = Vec::new();
        for replica in &state.proposal.to_policy.replicas {
            if *replica == local.0 {
                continue;
            }
            let Some(vote) = state.votes.get(replica) else {
                continue;
            };
            if vote.accepted {
                continue;
            }
            report.replicas_examined += 1;
            if vote.tail > state.proposal.candidate_tail {
                report.ahead_replicas += 1;
                continue;
            }
            if vote.tail == state.proposal.candidate_tail {
                continue;
            }

            let records = self.fabric_stream_read(
                stream,
                vote.tail.saturating_add(1),
                max_records_per_replica,
            )?;
            let records: Vec<FabricStreamEpochRepairRecord> = records
                .into_iter()
                .take_while(|record| record.sequence <= state.proposal.candidate_tail)
                .map(|record| FabricStreamEpochRepairRecord {
                    sequence: record.sequence,
                    payload: record.payload,
                })
                .collect();
            if records.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Fabric epoch repair could not find the candidate's missing durable records",
                ));
            }
            batches.push((
                NodeId(*replica),
                FabricStreamEpochRepairBatch {
                    stream: stream.to_string(),
                    proposal: state.proposal.clone(),
                    target: *replica,
                    records,
                },
            ));
        }

        let cluster = self.distributed.cluster.as_ref().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                "Fabric epoch repair requires cluster membership",
            )
        })?;
        let mut dispatches = Vec::new();
        for (target, batch) in batches {
            if cluster.is_removed(target) {
                report.unavailable_replicas += 1;
                continue;
            }
            let Some(address) = cluster
                .get_node(target)
                .filter(|info| matches!(info.status, NodeStatus::Healthy | NodeStatus::Joining))
                .map(|info| info.address)
            else {
                report.unavailable_replicas += 1;
                continue;
            };
            report.records_dispatched += batch.records.len();
            dispatches.push((target, address, batch.to_wire_bytes()?));
        }

        let transport = self.distributed.transport.as_mut().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                "Fabric epoch repair requires network transport",
            )
        })?;
        for (target, address, bytes) in dispatches {
            transport.send(
                target,
                address,
                system_packet(FABRIC_STREAM_EPOCH_REPAIR_BEHAVIOR, local, bytes),
            );
        }
        Ok(report)
    }

    pub(crate) fn fabric_stream_apply_epoch_repair_from_cluster(
        &mut self,
        batch: &FabricStreamEpochRepairBatch,
        sender: NodeId,
        cluster: &ClusterState,
    ) -> io::Result<FabricStreamEpochVoteState> {
        validate_proposal_shape(&batch.stream, &batch.proposal)?;
        let local = self.distributed.node_id.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                "Fabric epoch repair requires distribution",
            )
        })?;
        if sender.0 != batch.proposal.to_policy.leader
            || batch.target != local.0
            || !batch.proposal.to_policy.replicas.contains(&local.0)
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "unauthorized Fabric epoch repair batch",
            ));
        }

        let current = self
            .fabric_stream_replication_policy(&batch.stream)?
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "Fabric stream replication policy is not established",
                )
            })?;
        if current != batch.proposal.from_policy {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Fabric epoch repair source policy differs from local durable policy",
            ));
        }

        let placement = compute_stream_placement(
            local,
            Some(cluster),
            &batch.stream,
            batch.proposal.to_policy.partition,
            batch.proposal.to_policy.replication_factor,
        )?;
        if policy_from_placement(&placement, batch.proposal.to_policy.epoch)
            != batch.proposal.to_policy
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Fabric epoch repair no longer matches current placement",
            ));
        }

        if let Some(promise) = self.fabric_stream_epoch_promise(&batch.stream)? {
            if promise.epoch > batch.proposal.to_policy.epoch
                || (promise.epoch == batch.proposal.to_policy.epoch
                    && promise.proposal_hash != batch.proposal.proposal_hash)
            {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "Fabric epoch repair is fenced by a newer or conflicting promise",
                ));
            }
        }

        let local_tail = self
            .fabric_stream_info(&batch.stream)?
            .last_sequence
            .unwrap_or(0);
        if batch.records.is_empty()
            || batch
                .records
                .last()
                .map(|record| record.sequence > batch.proposal.candidate_tail)
                .unwrap_or(true)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Fabric epoch repair batch exceeds the proposed candidate prefix",
            ));
        }

        let mut batch_expected = batch.records[0].sequence;
        for record in &batch.records {
            if record.sequence != batch_expected {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Fabric epoch repair batch contains a sequence gap",
                ));
            }
            batch_expected = batch_expected.saturating_add(1);
        }

        let mut expected_new = local_tail.saturating_add(1);
        for record in &batch.records {
            if record.sequence <= local_tail {
                let existing = self
                    .fabric_stream_read(&batch.stream, record.sequence, 1)?
                    .into_iter()
                    .next()
                    .filter(|existing| existing.sequence == record.sequence);
                match existing {
                    Some(existing) if existing.payload == record.payload => continue,
                    _ => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "Fabric epoch repair retry conflicts with local durable data",
                        ));
                    }
                }
            }
            if record.sequence != expected_new {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Fabric epoch repair does not begin at the next local sequence",
                ));
            }
            self.fabric_stream_apply_transition_repair_record(
                &batch.stream,
                record.sequence,
                &record.payload,
            )?;
            expected_new = expected_new.saturating_add(1);
        }

        self.fabric_stream_evaluate_epoch_prepare(&batch.stream, &batch.proposal, sender, cluster)
    }

    pub(crate) fn fabric_stream_evaluate_epoch_prepare(
        &mut self,
        stream: &str,
        proposal: &FabricStreamEpochProposalState,
        sender: NodeId,
        cluster: &ClusterState,
    ) -> io::Result<FabricStreamEpochVoteState> {
        validate_proposal_shape(stream, proposal)?;
        if proposal.to_policy.leader != sender.0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Fabric epoch prepare sender is not the proposed leader",
            ));
        }

        let local = self.distributed.node_id.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                "Fabric epoch vote requires distribution",
            )
        })?;
        let current = self
            .fabric_stream_replication_policy(stream)?
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "Fabric stream replication policy is not established",
                )
            })?;
        if current != proposal.from_policy {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Fabric epoch prepare source policy differs from local durable policy",
            ));
        }
        if !proposal.from_policy.replicas.contains(&local.0) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "local node is not an old-policy Fabric replica",
            ));
        }

        let placement = compute_stream_placement(
            local,
            Some(cluster),
            stream,
            proposal.to_policy.partition,
            proposal.to_policy.replication_factor,
        )?;
        if policy_from_placement(&placement, proposal.to_policy.epoch) != proposal.to_policy {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Fabric epoch prepare does not match current deterministic placement",
            ));
        }

        let info = self.fabric_stream_info(stream)?;
        let local_tail = info.last_sequence.unwrap_or(0);
        let accepted = local_tail == proposal.candidate_tail;
        if accepted {
            self.fabric_stream_promise_epoch(
                stream,
                proposal.to_policy.epoch,
                &proposal.proposal_hash,
            )?;
        }

        Ok(FabricStreamEpochVoteState {
            voter: local.0,
            epoch: proposal.to_policy.epoch,
            proposal_hash: proposal.proposal_hash.clone(),
            tail: local_tail,
            committed_sequence: info.committed_sequence,
            accepted,
        })
    }

    pub(crate) fn fabric_stream_record_epoch_vote_from_cluster(
        &mut self,
        vote: FabricStreamEpochVote,
        sender: NodeId,
    ) -> io::Result<FabricStreamEpochVoteOutcome> {
        if vote.vote.voter != sender.0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Fabric epoch vote identity does not match transport sender",
            ));
        }

        let state = self.fabric_stream_record_epoch_vote(&vote.stream, vote.vote)?;
        if state.finalized {
            self.fabric_stream_complete_finalized_epoch_transition(&vote.stream, &state)?;
            return Ok(FabricStreamEpochVoteOutcome {
                status: status_from_state(&state),
                commit: Some(commit_from_state(&vote.stream, &state)?),
            });
        }

        let quorum = old_quorum(&state.proposal.from_policy);
        let affirmative: Vec<&FabricStreamEpochVoteState> = state
            .votes
            .values()
            .filter(|vote| vote.accepted && vote.tail == state.proposal.candidate_tail)
            .collect();
        let affirmative_ids: HashSet<u64> = affirmative.iter().map(|vote| vote.voter).collect();
        let new_replicas_ready = state
            .proposal
            .to_policy
            .replicas
            .iter()
            .all(|node| affirmative_ids.contains(node));

        if affirmative.len() < quorum || !new_replicas_ready {
            return Ok(FabricStreamEpochVoteOutcome {
                status: status_from_state(&state),
                commit: None,
            });
        }

        let current_committed = self.fabric_stream_committed_sequence(&vote.stream)?;
        if current_committed > state.proposal.candidate_tail {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Fabric candidate committed boundary is ahead of its durable tail",
            ));
        }

        // Every affirmative voter has the exact candidate tail. An old-policy
        // majority therefore durably contains the complete candidate prefix.
        let quorum_committed = state.proposal.candidate_tail;
        let finalized =
            self.fabric_stream_finalize_epoch_transition_state(&vote.stream, quorum_committed)?;

        self.fabric_stream_complete_finalized_epoch_transition(&vote.stream, &finalized)?;

        let commit = commit_from_state(&vote.stream, &finalized)?;
        Ok(FabricStreamEpochVoteOutcome {
            status: status_from_state(&finalized),
            commit: Some(commit),
        })
    }

    fn fabric_stream_complete_finalized_epoch_transition(
        &mut self,
        stream: &str,
        state: &FabricStreamEpochTransitionState,
    ) -> io::Result<()> {
        if !state.finalized {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "Fabric epoch transition is not finalized",
            ));
        }
        self.fabric_stream_install_epoch_policy(
            stream,
            &state.proposal.from_policy,
            &state.proposal.to_policy,
            &state.proposal.proposal_hash,
        )?;
        let current_committed = self.fabric_stream_committed_sequence(stream)?;
        if state.quorum_committed_sequence > current_committed {
            self.fabric_stream_commit_through(stream, state.quorum_committed_sequence)?;
        }
        self.fabric_stream_retire_old_epoch_pending(
            stream,
            state.proposal.from_policy.partition,
            state.quorum_committed_sequence,
        )
    }

    pub(crate) fn fabric_stream_apply_epoch_commit_from_cluster(
        &mut self,
        commit: &FabricStreamEpochCommit,
        sender: NodeId,
        cluster: &ClusterState,
    ) -> io::Result<()> {
        validate_proposal_shape(&commit.stream, &commit.proposal)?;
        if commit.proposal.to_policy.leader != sender.0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Fabric epoch commit sender is not the new leader",
            ));
        }

        let local = self.distributed.node_id.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                "Fabric epoch commit requires distribution",
            )
        })?;
        if !commit.proposal.to_policy.replicas.contains(&local.0) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "local node is not a replica in the new Fabric policy",
            ));
        }

        let voter_set: HashSet<u64> = commit.affirmative_voters.iter().copied().collect();
        if voter_set.len() != commit.affirmative_voters.len()
            || voter_set.len() < old_quorum(&commit.proposal.from_policy)
            || !voter_set
                .iter()
                .all(|node| commit.proposal.from_policy.replicas.contains(node))
            || !commit
                .proposal
                .to_policy
                .replicas
                .iter()
                .all(|node| voter_set.contains(node))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Fabric epoch commit has an invalid quorum certificate",
            ));
        }

        let placement = compute_stream_placement(
            local,
            Some(cluster),
            &commit.stream,
            commit.proposal.to_policy.partition,
            commit.proposal.to_policy.replication_factor,
        )?;
        if policy_from_placement(&placement, commit.proposal.to_policy.epoch)
            != commit.proposal.to_policy
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Fabric epoch commit no longer matches current placement",
            ));
        }

        let promise = self
            .fabric_stream_epoch_promise(&commit.stream)?
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "Fabric epoch commit requires a local durable vote promise",
                )
            })?;
        if promise.epoch != commit.proposal.to_policy.epoch
            || promise.proposal_hash != commit.proposal.proposal_hash
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Fabric epoch commit does not match local durable promise",
            ));
        }

        let tail = self
            .fabric_stream_info(&commit.stream)?
            .last_sequence
            .unwrap_or(0);
        if tail != commit.committed_sequence || tail != commit.proposal.candidate_tail {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "Fabric epoch commit requires the exact quorum-certified tail",
            ));
        }

        self.fabric_stream_install_epoch_policy(
            &commit.stream,
            &commit.proposal.from_policy,
            &commit.proposal.to_policy,
            &commit.proposal.proposal_hash,
        )?;
        let current_committed = self.fabric_stream_committed_sequence(&commit.stream)?;
        if commit.committed_sequence > current_committed {
            self.fabric_stream_commit_through(&commit.stream, commit.committed_sequence)?;
        }
        self.fabric_stream_retire_old_epoch_pending(
            &commit.stream,
            commit.proposal.from_policy.partition,
            commit.committed_sequence,
        )
    }

    fn fabric_stream_dispatch_epoch_prepare(
        &mut self,
        prepare: FabricStreamEpochPrepare,
    ) -> io::Result<()> {
        let local = self.distributed.node_id.ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotConnected, "distribution is not enabled")
        })?;
        if prepare.stream.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Fabric epoch prepare stream cannot be empty",
            ));
        }
        let bytes = prepare.to_wire_bytes()?;
        let cluster = self.distributed.cluster.as_ref().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                "cluster membership is unavailable",
            )
        })?;
        let mut targets = Vec::new();
        for node in &prepare.proposal.from_policy.replicas {
            let node = NodeId(*node);
            if node == local {
                continue;
            }
            if cluster.is_removed(node) {
                continue;
            }
            if let Some(info) = cluster.get_node(node) {
                if matches!(info.status, NodeStatus::Healthy | NodeStatus::Joining) {
                    targets.push((node, info.address));
                }
            }
        }
        let transport = self.distributed.transport.as_mut().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                "network transport is unavailable",
            )
        })?;
        for (node, address) in targets {
            transport.send(
                node,
                address,
                system_packet(FABRIC_STREAM_EPOCH_PREPARE_BEHAVIOR, local, bytes.clone()),
            );
        }
        Ok(())
    }

    fn fabric_stream_dispatch_epoch_commit(
        &mut self,
        commit: &FabricStreamEpochCommit,
    ) -> io::Result<()> {
        let local = self.distributed.node_id.ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotConnected, "distribution is not enabled")
        })?;
        let bytes = commit.to_wire_bytes()?;
        let cluster = self.distributed.cluster.as_ref().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                "cluster membership is unavailable",
            )
        })?;
        let mut targets = Vec::new();
        for node in &commit.proposal.to_policy.replicas {
            let node = NodeId(*node);
            if node == local {
                continue;
            }
            if let Some(info) = cluster.get_node(node) {
                if matches!(info.status, NodeStatus::Healthy | NodeStatus::Joining) {
                    targets.push((node, info.address));
                }
            }
        }
        let transport = self.distributed.transport.as_mut().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                "network transport is unavailable",
            )
        })?;
        for (node, address) in targets {
            transport.send(
                node,
                address,
                system_packet(FABRIC_STREAM_EPOCH_COMMIT_BEHAVIOR, local, bytes.clone()),
            );
        }
        Ok(())
    }
}

fn validate_proposal_shape(
    stream: &str,
    proposal: &FabricStreamEpochProposalState,
) -> io::Result<()> {
    if stream.is_empty()
        || proposal.proposal_hash.is_empty()
        || proposal.to_policy.epoch <= proposal.from_policy.epoch
        || proposal.to_policy.partition != proposal.from_policy.partition
        || proposal.to_policy.replication_factor == 0
        || proposal.to_policy.replicas.len() != proposal.to_policy.replication_factor
        || !proposal
            .to_policy
            .replicas
            .contains(&proposal.to_policy.leader)
        || !proposal
            .to_policy
            .replicas
            .iter()
            .all(|node| proposal.from_policy.replicas.contains(node))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid Fabric epoch transition proposal",
        ));
    }
    let expected = epoch_proposal_hash(
        stream,
        &proposal.from_policy,
        &proposal.to_policy,
        proposal.candidate_tail,
    );
    if expected != proposal.proposal_hash {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Fabric epoch proposal hash mismatch",
        ));
    }
    Ok(())
}

fn policy_from_placement(
    placement: &FabricStreamPlacement,
    epoch: u64,
) -> FabricStreamReplicationPolicy {
    FabricStreamReplicationPolicy {
        partition: placement.partition,
        epoch,
        leader: placement.leader.0,
        membership_fingerprint: placement.membership_fingerprint,
        replication_factor: placement.replicas.len(),
        replicas: placement.replicas.iter().map(|node| node.0).collect(),
    }
}

fn epoch_proposal_hash(
    stream: &str,
    from: &FabricStreamReplicationPolicy,
    to: &FabricStreamReplicationPolicy,
    candidate_tail: u64,
) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"nulang-fabric-stream-epoch-transition-v1");
    hasher.update(&(stream.len() as u64).to_be_bytes());
    hasher.update(stream.as_bytes());
    hash_policy(&mut hasher, from);
    hash_policy(&mut hasher, to);
    hasher.update(&candidate_tail.to_be_bytes());
    hasher.finalize().to_hex().to_string()
}

fn hash_policy(hasher: &mut blake3::Hasher, policy: &FabricStreamReplicationPolicy) {
    hasher.update(&policy.partition.to_be_bytes());
    hasher.update(&policy.epoch.to_be_bytes());
    hasher.update(&policy.leader.to_be_bytes());
    hasher.update(&policy.membership_fingerprint.to_be_bytes());
    hasher.update(&(policy.replication_factor as u64).to_be_bytes());
    for replica in &policy.replicas {
        hasher.update(&replica.to_be_bytes());
    }
}

fn old_quorum(policy: &FabricStreamReplicationPolicy) -> usize {
    policy.replication_factor / 2 + 1
}

fn status_from_state(
    state: &FabricStreamEpochTransitionState,
) -> FabricStreamEpochTransitionStatus {
    let affirmative_votes = state.votes.values().filter(|vote| vote.accepted).count();
    let rejected_votes = state.votes.values().filter(|vote| !vote.accepted).count();
    FabricStreamEpochTransitionStatus {
        from_epoch: state.proposal.from_policy.epoch,
        to_epoch: state.proposal.to_policy.epoch,
        affirmative_votes,
        rejected_votes,
        quorum: old_quorum(&state.proposal.from_policy),
        finalized: state.finalized,
        committed_sequence: state.quorum_committed_sequence,
    }
}

fn commit_from_state(
    stream: &str,
    state: &FabricStreamEpochTransitionState,
) -> io::Result<FabricStreamEpochCommit> {
    if !state.finalized {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "Fabric epoch transition has not reached quorum",
        ));
    }
    let affirmative_voters = state
        .votes
        .values()
        .filter(|vote| vote.accepted && vote.tail == state.proposal.candidate_tail)
        .map(|vote| vote.voter)
        .collect();
    Ok(FabricStreamEpochCommit {
        stream: stream.to_string(),
        proposal: state.proposal.clone(),
        affirmative_voters,
        committed_sequence: state.quorum_committed_sequence,
    })
}

fn system_packet(behavior: &str, sender_node: NodeId, bytes: Vec<u8>) -> Packet {
    Packet::ActorMessage {
        target_actor: 0,
        behavior_name: behavior.to_string(),
        content_hash: None,
        payload: Vec::new(),
        string_table: Vec::new(),
        object_table: vec![(0, bytes)],
        sender_actor: 0,
        sender_node,
        priority: MessagePriority::System,
        trace_id: None,
    }
}

fn json_error(error: serde_json::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}
