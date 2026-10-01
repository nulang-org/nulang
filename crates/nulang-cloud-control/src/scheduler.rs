use crate::model::{
    BlockedPlacement, DeploymentSpec, Evaluation, NodeDescriptor, NodeRejection, NodeState,
    ObservedAllocation, PlacementPlan, PlanError, PlannedAllocation, Rejection, RejectionCode,
    ScoreBreakdown, SupersededAllocation, SupersededReason,
};
use std::collections::BTreeMap;

/// Build a deterministic placement plan from desired state and an observed cluster snapshot.
///
/// This function is deliberately pure: it never starts/stops workloads and never acquires
/// provider capacity. Callers can persist/validate the returned plan before committing it.
pub fn plan_evaluation(
    evaluation: &Evaluation,
    deployment: &DeploymentSpec,
    nodes: &[NodeDescriptor],
    allocations: &[ObservedAllocation],
) -> Result<PlacementPlan, PlanError> {
    if evaluation.deployment_id != deployment.deployment_id {
        return Err(PlanError::DeploymentMismatch {
            evaluation: evaluation.deployment_id.clone(),
            deployment: deployment.deployment_id.clone(),
        });
    }
    if evaluation.revision != deployment.revision {
        return Err(PlanError::RevisionMismatch {
            evaluation: evaluation.revision,
            deployment: deployment.revision,
        });
    }

    let mut working_nodes: BTreeMap<u64, NodeDescriptor> = nodes
        .iter()
        .cloned()
        .map(|node| (node.node_id, node))
        .collect();

    let relevant = allocations
        .iter()
        .filter(|allocation| allocation.deployment_id == deployment.deployment_id)
        .cloned()
        .collect::<Vec<_>>();

    let mut by_replica: BTreeMap<u32, Vec<ObservedAllocation>> = BTreeMap::new();
    for allocation in relevant {
        by_replica
            .entry(allocation.replica)
            .or_default()
            .push(allocation);
    }

    let mut retained = Vec::new();
    let mut superseded = Vec::new();
    let mut node_replica_counts: BTreeMap<u64, u32> = BTreeMap::new();
    let mut zone_replica_counts: BTreeMap<String, u32> = BTreeMap::new();
    let mut next_epoch_by_replica = BTreeMap::new();

    // Establish the authoritative allocation for each replica before making
    // new placement decisions. A lower epoch is fenced even if it still says Running.
    for replica in 0..deployment.replicas {
        let existing = by_replica.get(&replica).cloned().unwrap_or_default();
        let max_epoch = existing
            .iter()
            .map(|allocation| allocation.epoch)
            .max()
            .unwrap_or(0);
        next_epoch_by_replica.insert(replica, max_epoch.saturating_add(1));

        let authoritative = existing
            .iter()
            .filter(|allocation| allocation.epoch == max_epoch)
            .max_by_key(|allocation| allocation.node_id)
            .cloned();

        for allocation in existing
            .iter()
            .filter(|allocation| allocation.state.is_active())
        {
            if authoritative
                .as_ref()
                .map(|current| {
                    current.epoch != allocation.epoch || current.node_id != allocation.node_id
                })
                .unwrap_or(true)
            {
                superseded.push(SupersededAllocation {
                    allocation: allocation.clone(),
                    reason: SupersededReason::Duplicate,
                });
            }
        }

        let Some(current) = authoritative else {
            continue;
        };
        if !current.state.is_active() {
            continue;
        }
        if current.revision != deployment.revision {
            superseded.push(SupersededAllocation {
                allocation: current,
                reason: SupersededReason::StaleRevision,
            });
            continue;
        }

        // Absence from a point-in-time node snapshot is not proof of death.
        // Fail closed and retain ownership until membership explicitly marks
        // the node Removed (or an operator intentionally drains it).
        let valid = working_nodes
            .get(&current.node_id)
            .map(|node| retention_rejections(deployment, node).is_empty())
            .unwrap_or(true);
        if !valid {
            superseded.push(SupersededAllocation {
                allocation: current,
                reason: SupersededReason::PlacementInvalid,
            });
            continue;
        }

        *node_replica_counts.entry(current.node_id).or_insert(0) += 1;
        if let Some(zone) = working_nodes
            .get(&current.node_id)
            .and_then(|node| node.zone.clone())
        {
            *zone_replica_counts.entry(zone).or_insert(0) += 1;
        }
        retained.push(current);
    }

    // Active allocations above the desired replica count are explicit scale-down work.
    for (replica, existing) in &by_replica {
        if *replica < deployment.replicas {
            continue;
        }
        for allocation in existing
            .iter()
            .filter(|allocation| allocation.state.is_active())
        {
            superseded.push(SupersededAllocation {
                allocation: allocation.clone(),
                reason: SupersededReason::ScaleDown,
            });
        }
    }

    let retained_by_replica = retained
        .iter()
        .map(|allocation| (allocation.replica, allocation.clone()))
        .collect::<BTreeMap<_, _>>();

    let mut placements = Vec::new();
    let mut blocked = None;

    for replica in 0..deployment.replicas {
        if retained_by_replica.contains_key(&replica) {
            continue;
        }

        let mut feasible = Vec::new();
        let mut node_rejections = Vec::new();
        for node in working_nodes.values() {
            let reasons = placement_rejections(
                deployment,
                node,
                node_replica_counts.get(&node.node_id).copied().unwrap_or(0),
            );
            if reasons.is_empty() {
                feasible.push((
                    node.node_id,
                    score_node(deployment, node, &zone_replica_counts, &node_replica_counts),
                ));
            } else {
                node_rejections.push(NodeRejection {
                    node_id: node.node_id,
                    reasons,
                });
            }
        }

        if feasible.is_empty() {
            let mut rejection_counts = BTreeMap::new();
            for rejected in &node_rejections {
                for reason in &rejected.reasons {
                    *rejection_counts.entry(reason.code).or_insert(0) += 1;
                }
            }
            blocked = Some(BlockedPlacement {
                unscheduled_replicas: (replica..deployment.replicas)
                    .filter(|candidate| !retained_by_replica.contains_key(candidate))
                    .collect(),
                considered_nodes: working_nodes.len(),
                rejection_counts,
                node_rejections,
            });
            break;
        }

        feasible.sort_by(|(left_id, left_score), (right_id, right_score)| {
            right_score
                .total()
                .cmp(&left_score.total())
                .then_with(|| left_id.cmp(right_id))
        });
        let (node_id, score) = feasible[0];

        let node = working_nodes
            .get_mut(&node_id)
            .expect("scored node must exist in working set");
        let reserved = node.resources.reserve(&deployment.resources);
        debug_assert!(reserved, "feasibility and reservation must agree");

        *node_replica_counts.entry(node_id).or_insert(0) += 1;
        if let Some(zone) = node.zone.clone() {
            *zone_replica_counts.entry(zone).or_insert(0) += 1;
        }

        placements.push(PlannedAllocation {
            replica,
            node_id,
            epoch: next_epoch_by_replica.get(&replica).copied().unwrap_or(1),
            score,
        });
    }

    retained.sort_by_key(|allocation| allocation.replica);
    placements.sort_by_key(|allocation| allocation.replica);
    superseded.sort_by(|left, right| {
        left.allocation
            .replica
            .cmp(&right.allocation.replica)
            .then_with(|| left.allocation.epoch.cmp(&right.allocation.epoch))
            .then_with(|| left.allocation.node_id.cmp(&right.allocation.node_id))
    });

    Ok(PlacementPlan {
        evaluation_id: evaluation.evaluation_id.clone(),
        deployment_id: deployment.deployment_id.clone(),
        revision: deployment.revision,
        retained,
        placements,
        superseded,
        blocked,
    })
}

fn retention_rejections(deployment: &DeploymentSpec, node: &NodeDescriptor) -> Vec<Rejection> {
    // Suspicion/unreachability is not a fencing event. Replacing an allocation
    // while its old node may still be alive can create two live owners. Keep
    // the current epoch until the node is confirmed Removed or deliberately
    // Draining. Once the node is healthy again, normal placement constraints
    // may trigger a controlled replacement.
    if matches!(node.state, NodeState::Suspect | NodeState::Unreachable) {
        return Vec::new();
    }
    common_rejections(deployment, node, false, 0)
}

fn placement_rejections(
    deployment: &DeploymentSpec,
    node: &NodeDescriptor,
    deployment_replicas_on_node: u32,
) -> Vec<Rejection> {
    common_rejections(deployment, node, true, deployment_replicas_on_node)
}

fn common_rejections(
    deployment: &DeploymentSpec,
    node: &NodeDescriptor,
    check_resources: bool,
    deployment_replicas_on_node: u32,
) -> Vec<Rejection> {
    let mut reasons = Vec::new();
    let constraints = &deployment.constraints;

    if node.state != NodeState::Ready {
        reasons.push(Rejection {
            code: RejectionCode::NodeNotReady,
            detail: format!("node state is {:?}", node.state),
        });
    }

    if let Some(required) = constraints.architecture {
        if node.architecture != required {
            reasons.push(Rejection {
                code: RejectionCode::ArchitectureMismatch,
                detail: format!(
                    "requires {:?}, node provides {:?}",
                    required, node.architecture
                ),
            });
        }
    }

    if !constraints.allowed_regions.is_empty()
        && !constraints
            .allowed_regions
            .iter()
            .any(|region| region == &node.region)
    {
        reasons.push(Rejection {
            code: RejectionCode::RegionNotAllowed,
            detail: format!("region {} is not allowed", node.region),
        });
    }

    if !constraints.allowed_zones.is_empty() {
        let allowed = node
            .zone
            .as_ref()
            .map(|zone| {
                constraints
                    .allowed_zones
                    .iter()
                    .any(|candidate| candidate == zone)
            })
            .unwrap_or(false);
        if !allowed {
            reasons.push(Rejection {
                code: RejectionCode::ZoneNotAllowed,
                detail: format!(
                    "zone {} is not allowed",
                    node.zone.as_deref().unwrap_or("<none>")
                ),
            });
        }
    }

    if node.trust_tier < constraints.min_trust_tier {
        reasons.push(Rejection {
            code: RejectionCode::TrustTierTooLow,
            detail: format!(
                "requires {:?}, node provides {:?}",
                constraints.min_trust_tier, node.trust_tier
            ),
        });
    }

    for (key, expected) in &constraints.required_labels {
        if node.labels.get(key) != Some(expected) {
            reasons.push(Rejection {
                code: RejectionCode::MissingLabel,
                detail: format!("requires label {key}={expected}"),
            });
        }
    }

    for capability in &constraints.required_capabilities {
        if !node.capabilities.contains(capability) {
            reasons.push(Rejection {
                code: RejectionCode::MissingCapability,
                detail: format!("requires capability {capability}"),
            });
        }
    }

    if let Some(maximum) = constraints.max_replicas_per_node {
        if deployment_replicas_on_node >= maximum {
            reasons.push(Rejection {
                code: RejectionCode::MaxReplicasPerNode,
                detail: format!("node already hosts {deployment_replicas_on_node} replicas"),
            });
        }
    }

    if check_resources {
        if node.resources.cpu_millis_available < deployment.resources.cpu_millis {
            reasons.push(Rejection {
                code: RejectionCode::InsufficientCpu,
                detail: format!(
                    "requires {}m CPU, {}m available",
                    deployment.resources.cpu_millis, node.resources.cpu_millis_available
                ),
            });
        }
        if node.resources.memory_mib_available < deployment.resources.memory_mib {
            reasons.push(Rejection {
                code: RejectionCode::InsufficientMemory,
                detail: format!(
                    "requires {} MiB memory, {} MiB available",
                    deployment.resources.memory_mib, node.resources.memory_mib_available
                ),
            });
        }
        if deployment.resources.accelerator.is_some() && !node.resources.fits(&deployment.resources)
        {
            // CPU or memory may also be insufficient; keeping accelerator as a
            // separate reason makes the explain output actionable.
            let cpu_and_memory_fit = node.resources.cpu_millis_available
                >= deployment.resources.cpu_millis
                && node.resources.memory_mib_available >= deployment.resources.memory_mib;
            if cpu_and_memory_fit {
                reasons.push(Rejection {
                    code: RejectionCode::InsufficientAccelerator,
                    detail: "accelerator requirement cannot be satisfied".into(),
                });
            }
        }
    }

    reasons
}

fn score_node(
    deployment: &DeploymentSpec,
    node: &NodeDescriptor,
    zone_replica_counts: &BTreeMap<String, u32>,
    node_replica_counts: &BTreeMap<u64, u32>,
) -> ScoreBreakdown {
    let preferences = &deployment.preferences;

    let region_locality = preferences
        .preferred_regions
        .iter()
        .position(|region| region == &node.region)
        .map(|index| {
            let strength = preferences.preferred_regions.len().saturating_sub(index) as i64;
            strength * 1_000
        })
        .unwrap_or(0);

    let zone_spread = if preferences.spread_by_zone {
        node.zone
            .as_ref()
            .map(|zone| -(zone_replica_counts.get(zone).copied().unwrap_or(0) as i64) * 100)
            .unwrap_or(0)
    } else {
        0
    };

    let node_spread = -(node_replica_counts.get(&node.node_id).copied().unwrap_or(0) as i64) * 500;

    let headroom = if preferences.prefer_headroom {
        let cpu_after = node
            .resources
            .cpu_millis_available
            .saturating_sub(deployment.resources.cpu_millis);
        let memory_after = node
            .resources
            .memory_mib_available
            .saturating_sub(deployment.resources.memory_mib);
        percent(cpu_after as u64, node.resources.cpu_millis_total as u64) as i64
            + percent(memory_after, node.resources.memory_mib_total) as i64
    } else {
        0
    };

    ScoreBreakdown {
        region_locality,
        zone_spread,
        node_spread,
        headroom,
    }
}

fn percent(value: u64, total: u64) -> u32 {
    if total == 0 {
        return 0;
    }
    value.saturating_mul(100).saturating_div(total).min(100) as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        AllocationState, PlacementConstraints, PlacementPreferences, ResourceRequest,
    };
    use nulang_capacity::{Architecture, TrustTier};
    use std::collections::{BTreeMap, BTreeSet};

    fn node(id: u64, region: &str, zone: &str, cpu: u32, memory: u64) -> NodeDescriptor {
        NodeDescriptor {
            node_id: id,
            state: NodeState::Ready,
            region: region.into(),
            zone: Some(zone.into()),
            architecture: Architecture::X86_64,
            trust_tier: TrustTier::CloudProvider,
            labels: BTreeMap::new(),
            capabilities: BTreeSet::new(),
            resources: crate::model::NodeResources {
                cpu_millis_total: cpu,
                cpu_millis_available: cpu,
                memory_mib_total: memory,
                memory_mib_available: memory,
                accelerators: BTreeMap::new(),
            },
        }
    }

    fn deployment(replicas: u32) -> DeploymentSpec {
        DeploymentSpec {
            deployment_id: "api".into(),
            revision: 2,
            replicas,
            resources: ResourceRequest {
                cpu_millis: 1_000,
                memory_mib: 512,
                accelerator: None,
            },
            constraints: PlacementConstraints::default(),
            preferences: PlacementPreferences {
                preferred_regions: Vec::new(),
                spread_by_zone: true,
                prefer_headroom: true,
            },
        }
    }

    fn evaluation() -> Evaluation {
        Evaluation {
            evaluation_id: "eval-1".into(),
            deployment_id: "api".into(),
            revision: 2,
            cause: crate::model::EvaluationCause::DeploymentChanged,
        }
    }

    #[test]
    fn test_hard_constraints_block_before_scoring_and_explain_why() {
        let mut spec = deployment(1);
        spec.constraints.architecture = Some(Architecture::Arm64);
        spec.constraints.allowed_regions = vec!["eu-west".into()];

        let nodes = vec![
            node(1, "us-east", "a", 4_000, 4_096),
            node(2, "eu-west", "b", 500, 256),
        ];

        let plan = plan_evaluation(&evaluation(), &spec, &nodes, &[]).unwrap();
        let blocked = plan.blocked.expect("placement should be blocked");
        assert_eq!(blocked.unscheduled_replicas, vec![0]);
        assert_eq!(blocked.considered_nodes, 2);
        assert_eq!(
            blocked
                .rejection_counts
                .get(&RejectionCode::ArchitectureMismatch),
            Some(&2)
        );
        assert_eq!(
            blocked
                .rejection_counts
                .get(&RejectionCode::RegionNotAllowed),
            Some(&1)
        );
    }

    #[test]
    fn test_preferred_region_wins_before_headroom() {
        let mut spec = deployment(1);
        spec.preferences.preferred_regions = vec!["sa-east".into()];
        let nodes = vec![
            node(1, "us-east", "a", 32_000, 65_536),
            node(2, "sa-east", "b", 2_000, 2_048),
        ];

        let plan = plan_evaluation(&evaluation(), &spec, &nodes, &[]).unwrap();
        assert_eq!(plan.placements[0].node_id, 2);
    }

    #[test]
    fn test_replicas_spread_across_zones_deterministically() {
        let spec = deployment(2);
        let nodes = vec![
            node(1, "us-east", "a", 4_000, 4_096),
            node(2, "us-east", "a", 4_000, 4_096),
            node(3, "us-east", "b", 4_000, 4_096),
        ];

        let plan = plan_evaluation(&evaluation(), &spec, &nodes, &[]).unwrap();
        assert!(plan.is_complete());
        assert_eq!(plan.placements.len(), 2);
        assert_eq!(plan.placements[0].node_id, 1);
        assert_eq!(plan.placements[1].node_id, 3);
    }

    #[test]
    fn test_matching_allocation_is_retained_without_duplicate_placement() {
        let spec = deployment(1);
        let existing = ObservedAllocation {
            deployment_id: "api".into(),
            revision: 2,
            replica: 0,
            node_id: 1,
            epoch: 4,
            state: AllocationState::Running,
        };

        let plan = plan_evaluation(
            &evaluation(),
            &spec,
            &[node(1, "us-east", "a", 4_000, 4_096)],
            &[existing],
        )
        .unwrap();

        assert_eq!(plan.retained.len(), 1);
        assert!(plan.placements.is_empty());
        assert!(plan.superseded.is_empty());
    }

    #[test]
    fn test_stale_revision_is_replaced_with_incremented_fencing_epoch() {
        let spec = deployment(1);
        let mut old_node = node(1, "us-east", "a", 4_000, 4_096);
        old_node.state = NodeState::Draining;
        let existing = ObservedAllocation {
            deployment_id: "api".into(),
            revision: 1,
            replica: 0,
            node_id: 1,
            epoch: 7,
            state: AllocationState::Running,
        };

        let plan = plan_evaluation(
            &evaluation(),
            &spec,
            &[old_node, node(2, "us-east", "b", 4_000, 4_096)],
            &[existing],
        )
        .unwrap();

        assert_eq!(plan.placements[0].node_id, 2);
        assert_eq!(plan.placements[0].epoch, 8);
        assert_eq!(plan.superseded[0].reason, SupersededReason::StaleRevision);
    }

    #[test]
    fn test_capacity_exhaustion_returns_partial_plan_and_blocked_remainder() {
        let spec = deployment(2);
        let nodes = vec![node(1, "us-east", "a", 1_000, 512)];

        let plan = plan_evaluation(&evaluation(), &spec, &nodes, &[]).unwrap();
        assert_eq!(plan.placements.len(), 1);
        let blocked = plan.blocked.expect("second replica must block");
        assert_eq!(blocked.unscheduled_replicas, vec![1]);
        assert_eq!(
            blocked
                .rejection_counts
                .get(&RejectionCode::InsufficientCpu),
            Some(&1)
        );
        assert_eq!(
            blocked
                .rejection_counts
                .get(&RejectionCode::InsufficientMemory),
            Some(&1)
        );
    }

    #[test]
    fn test_max_replicas_per_node_is_a_hard_constraint() {
        let mut spec = deployment(2);
        spec.constraints.max_replicas_per_node = Some(1);
        spec.preferences.spread_by_zone = false;

        let nodes = vec![
            node(1, "us-east", "a", 8_000, 8_192),
            node(2, "us-east", "a", 8_000, 8_192),
        ];

        let plan = plan_evaluation(&evaluation(), &spec, &nodes, &[]).unwrap();
        assert_eq!(plan.placements.len(), 2);
        assert_ne!(plan.placements[0].node_id, plan.placements[1].node_id);
    }

    #[test]
    fn test_evaluation_revision_must_match_desired_state() {
        let spec = deployment(1);
        let mut eval = evaluation();
        eval.revision = 99;

        assert_eq!(
            plan_evaluation(&eval, &spec, &[], &[]),
            Err(PlanError::RevisionMismatch {
                evaluation: 99,
                deployment: 2,
            })
        );
    }

    #[test]
    fn test_lower_epoch_running_duplicate_is_superseded() {
        let spec = deployment(1);
        let allocations = vec![
            ObservedAllocation {
                deployment_id: "api".into(),
                revision: 2,
                replica: 0,
                node_id: 1,
                epoch: 4,
                state: AllocationState::Running,
            },
            ObservedAllocation {
                deployment_id: "api".into(),
                revision: 2,
                replica: 0,
                node_id: 2,
                epoch: 5,
                state: AllocationState::Running,
            },
        ];

        let plan = plan_evaluation(
            &evaluation(),
            &spec,
            &[
                node(1, "us-east", "a", 4_000, 4_096),
                node(2, "us-east", "b", 4_000, 4_096),
            ],
            &allocations,
        )
        .unwrap();

        assert_eq!(plan.retained[0].epoch, 5);
        assert_eq!(plan.superseded.len(), 1);
        assert_eq!(plan.superseded[0].allocation.epoch, 4);
        assert_eq!(plan.superseded[0].reason, SupersededReason::Duplicate);
    }

    #[test]
    fn test_suspect_allocation_is_retained_to_avoid_split_brain() {
        let spec = deployment(1);
        let mut suspect = node(1, "us-east", "a", 4_000, 4_096);
        suspect.state = NodeState::Suspect;
        let existing = ObservedAllocation {
            deployment_id: "api".into(),
            revision: 2,
            replica: 0,
            node_id: 1,
            epoch: 9,
            state: AllocationState::Running,
        };

        let plan = plan_evaluation(
            &evaluation(),
            &spec,
            &[suspect, node(2, "us-east", "b", 4_000, 4_096)],
            &[existing],
        )
        .unwrap();

        assert_eq!(plan.retained.len(), 1);
        assert_eq!(plan.retained[0].epoch, 9);
        assert!(plan.placements.is_empty());
        assert!(plan.superseded.is_empty());
    }

    #[test]
    fn test_removed_allocation_is_replaced_with_next_epoch() {
        let spec = deployment(1);
        let mut removed = node(1, "us-east", "a", 4_000, 4_096);
        removed.state = NodeState::Removed;
        let existing = ObservedAllocation {
            deployment_id: "api".into(),
            revision: 2,
            replica: 0,
            node_id: 1,
            epoch: 9,
            state: AllocationState::Running,
        };

        let plan = plan_evaluation(
            &evaluation(),
            &spec,
            &[removed, node(2, "us-east", "b", 4_000, 4_096)],
            &[existing],
        )
        .unwrap();

        assert_eq!(plan.placements.len(), 1);
        assert_eq!(plan.placements[0].node_id, 2);
        assert_eq!(plan.placements[0].epoch, 10);
        assert_eq!(
            plan.superseded[0].reason,
            SupersededReason::PlacementInvalid
        );
    }

    #[test]
    fn test_missing_node_is_not_treated_as_confirmed_dead() {
        let spec = deployment(1);
        let existing = ObservedAllocation {
            deployment_id: "api".into(),
            revision: 2,
            replica: 0,
            node_id: 99,
            epoch: 3,
            state: AllocationState::Running,
        };

        let plan = plan_evaluation(
            &evaluation(),
            &spec,
            &[node(2, "us-east", "b", 4_000, 4_096)],
            &[existing],
        )
        .unwrap();

        assert_eq!(plan.retained.len(), 1);
        assert_eq!(plan.retained[0].node_id, 99);
        assert!(plan.placements.is_empty());
    }

    #[test]
    fn test_tie_breaks_on_stable_node_id() {
        let mut spec = deployment(1);
        spec.preferences.spread_by_zone = false;
        spec.preferences.prefer_headroom = false;

        let nodes = vec![
            node(9, "us-east", "a", 4_000, 4_096),
            node(3, "us-east", "b", 4_000, 4_096),
        ];

        let plan = plan_evaluation(&evaluation(), &spec, &nodes, &[]).unwrap();
        assert_eq!(plan.placements[0].node_id, 3);
    }
}
