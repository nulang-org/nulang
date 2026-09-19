//! Provider allocation reconciliation for Nulang Cloud.
//!
//! High-frequency heartbeats answer liveness and aggregate utilization.
//! Exact allocation identity is reported separately at a lower cadence so
//! reconciliation can detect missing, orphaned, retired, or stale instances
//! without making liveness messages grow with workload count.

use crate::state::{AllocationLedgerSnapshot, CapacityHeartbeatBook};
use crate::topology::{ResourceClass, TopologyError, TopologySnapshot};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservedAllocation {
    pub allocation_id: String,
    pub placement_token: String,
    pub resources: BTreeMap<ResourceClass, u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderAllocationReport {
    pub provider_id: String,
    pub topology_generation: u64,
    pub sequence: u64,
    pub observed_at_unix_ms: u64,
    pub active_allocations: Vec<ObservedAllocation>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AllocationReportBook {
    latest: BTreeMap<String, ProviderAllocationReport>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum AllocationReportError {
    #[error("allocation report provider id must not be empty")]
    EmptyProvider,
    #[error("allocation report references unknown provider {0}")]
    UnknownProvider(String),
    #[error(
        "allocation report topology generation {observed} does not match current generation {current}"
    )]
    StaleTopology { observed: u64, current: u64 },
    #[error(
        "allocation report sequence {observed} is not newer than current sequence {current} for provider {provider}"
    )]
    StaleSequence {
        provider: String,
        observed: u64,
        current: u64,
    },
    #[error("observed allocation id must not be empty")]
    EmptyAllocationId,
    #[error("observed allocation {0} has an empty placement token")]
    EmptyPlacementToken(String),
    #[error("observed allocation id {0} appears more than once")]
    DuplicateAllocation(String),
    #[error(
        "observed allocation {allocation_id} reports unknown resource {resource} on provider {provider}"
    )]
    UnknownResource {
        provider: String,
        allocation_id: String,
        resource: ResourceClass,
    },
    #[error(transparent)]
    Topology(#[from] TopologyError),
}

impl AllocationReportBook {
    pub fn apply(
        &mut self,
        topology: &TopologySnapshot,
        report: ProviderAllocationReport,
    ) -> Result<(), AllocationReportError> {
        topology.validate()?;

        if report.provider_id.trim().is_empty() {
            return Err(AllocationReportError::EmptyProvider);
        }
        if report.topology_generation != topology.generation {
            return Err(AllocationReportError::StaleTopology {
                observed: report.topology_generation,
                current: topology.generation,
            });
        }

        let provider = topology
            .provider(&report.provider_id)
            .ok_or_else(|| AllocationReportError::UnknownProvider(report.provider_id.clone()))?;

        let mut ids = BTreeSet::new();
        for allocation in &report.active_allocations {
            if allocation.allocation_id.trim().is_empty() {
                return Err(AllocationReportError::EmptyAllocationId);
            }
            if allocation.placement_token.trim().is_empty() {
                return Err(AllocationReportError::EmptyPlacementToken(
                    allocation.allocation_id.clone(),
                ));
            }
            if !ids.insert(allocation.allocation_id.as_str()) {
                return Err(AllocationReportError::DuplicateAllocation(
                    allocation.allocation_id.clone(),
                ));
            }
            for resource in allocation.resources.keys() {
                if !provider.inventory.contains_key(resource) {
                    return Err(AllocationReportError::UnknownResource {
                        provider: provider.id.clone(),
                        allocation_id: allocation.allocation_id.clone(),
                        resource: resource.clone(),
                    });
                }
            }
        }

        if let Some(current) = self.latest.get(&report.provider_id) {
            if current.topology_generation == report.topology_generation
                && report.sequence <= current.sequence
            {
                return Err(AllocationReportError::StaleSequence {
                    provider: report.provider_id.clone(),
                    observed: report.sequence,
                    current: current.sequence,
                });
            }
        }

        self.latest.insert(report.provider_id.clone(), report);
        Ok(())
    }

    pub fn latest(&self, provider_id: &str) -> Option<&ProviderAllocationReport> {
        self.latest.get(provider_id)
    }

    pub fn latest_fresh(
        &self,
        provider_id: &str,
        topology_generation: u64,
        now_unix_ms: u64,
        max_age_ms: u64,
    ) -> Option<&ProviderAllocationReport> {
        self.latest.get(provider_id).filter(|report| {
            report.topology_generation == topology_generation
                && report.observed_at_unix_ms <= now_unix_ms
                && now_unix_ms - report.observed_at_unix_ms <= max_age_ms
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    Orphan,
    Retired,
    PlacementTokenMismatch,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "action")]
pub enum ReconcileAction {
    StopUnexpected {
        allocation_id: String,
        observed_placement_token: String,
        reason: StopReason,
    },
    VerifyResourceDrift {
        allocation_id: String,
        placement_token: String,
        desired_resources: BTreeMap<ResourceClass, u64>,
        observed_resources: BTreeMap<ResourceClass, u64>,
    },
    EnsureDesired {
        allocation_id: String,
        placement_token: String,
        resources: BTreeMap<ResourceClass, u64>,
    },
}

impl ReconcileAction {
    fn allocation_id(&self) -> &str {
        match self {
            ReconcileAction::StopUnexpected { allocation_id, .. }
            | ReconcileAction::VerifyResourceDrift { allocation_id, .. }
            | ReconcileAction::EnsureDesired { allocation_id, .. } => allocation_id,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReconcilePlan {
    pub provider_id: String,
    pub topology_generation: u64,
    pub provider_generation: u64,
    pub report_sequence: u64,
    pub actions: Vec<ReconcileAction>,
}

impl ReconcilePlan {
    pub fn is_clean(&self) -> bool {
        self.actions.is_empty()
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ReconcileError {
    #[error("provider id must not be empty")]
    EmptyProvider,
    #[error("provider {0} does not exist in the current topology")]
    UnknownProvider(String),
    #[error("provider {0} is not live and ready; reconciliation fails closed")]
    ProviderNotReady(String),
    #[error("provider {0} has no fresh allocation inventory report")]
    MissingOrStaleReport(String),
    #[error(transparent)]
    Topology(#[from] TopologyError),
}

#[allow(clippy::too_many_arguments)]
pub fn reconcile_provider(
    topology: &TopologySnapshot,
    ledger: &AllocationLedgerSnapshot,
    heartbeats: &CapacityHeartbeatBook,
    reports: &AllocationReportBook,
    provider_id: &str,
    now_unix_ms: u64,
    max_heartbeat_age_ms: u64,
    max_report_age_ms: u64,
) -> Result<ReconcilePlan, ReconcileError> {
    topology.validate()?;

    if provider_id.trim().is_empty() {
        return Err(ReconcileError::EmptyProvider);
    }
    if topology.provider(provider_id).is_none() {
        return Err(ReconcileError::UnknownProvider(provider_id.to_string()));
    }
    if !heartbeats.is_schedulable(
        provider_id,
        topology.generation,
        now_unix_ms,
        max_heartbeat_age_ms,
    ) {
        return Err(ReconcileError::ProviderNotReady(provider_id.to_string()));
    }

    let report = reports
        .latest_fresh(
            provider_id,
            topology.generation,
            now_unix_ms,
            max_report_age_ms,
        )
        .ok_or_else(|| ReconcileError::MissingOrStaleReport(provider_id.to_string()))?;

    let desired = ledger.provider_snapshot(provider_id);
    let observed: BTreeMap<&str, &ObservedAllocation> = report
        .active_allocations
        .iter()
        .map(|allocation| (allocation.allocation_id.as_str(), allocation))
        .collect();

    let mut stops = Vec::new();
    let mut verifies = Vec::new();
    let mut ensures = Vec::new();

    for allocation in &report.active_allocations {
        match desired.allocations.get(&allocation.allocation_id) {
            Some(expected) if expected.placement_token != allocation.placement_token => {
                stops.push(ReconcileAction::StopUnexpected {
                    allocation_id: allocation.allocation_id.clone(),
                    observed_placement_token: allocation.placement_token.clone(),
                    reason: StopReason::PlacementTokenMismatch,
                });
                ensures.push(ReconcileAction::EnsureDesired {
                    allocation_id: expected.allocation_id.clone(),
                    placement_token: expected.placement_token.clone(),
                    resources: expected.resources.clone(),
                });
            }
            Some(expected) if expected.resources != allocation.resources => {
                verifies.push(ReconcileAction::VerifyResourceDrift {
                    allocation_id: allocation.allocation_id.clone(),
                    placement_token: allocation.placement_token.clone(),
                    desired_resources: expected.resources.clone(),
                    observed_resources: allocation.resources.clone(),
                });
            }
            Some(_) => {}
            None if desired
                .retired_allocation_ids
                .contains(&allocation.allocation_id) =>
            {
                stops.push(ReconcileAction::StopUnexpected {
                    allocation_id: allocation.allocation_id.clone(),
                    observed_placement_token: allocation.placement_token.clone(),
                    reason: StopReason::Retired,
                });
            }
            None => {
                stops.push(ReconcileAction::StopUnexpected {
                    allocation_id: allocation.allocation_id.clone(),
                    observed_placement_token: allocation.placement_token.clone(),
                    reason: StopReason::Orphan,
                });
            }
        }
    }

    for expected in desired.allocations.values() {
        if !observed.contains_key(expected.allocation_id.as_str()) {
            ensures.push(ReconcileAction::EnsureDesired {
                allocation_id: expected.allocation_id.clone(),
                placement_token: expected.placement_token.clone(),
                resources: expected.resources.clone(),
            });
        }
    }

    stops.sort_by(|left, right| left.allocation_id().cmp(right.allocation_id()));
    verifies.sort_by(|left, right| left.allocation_id().cmp(right.allocation_id()));
    ensures.sort_by(|left, right| left.allocation_id().cmp(right.allocation_id()));

    let mut actions = Vec::with_capacity(stops.len() + verifies.len() + ensures.len());
    actions.extend(stops);
    actions.extend(verifies);
    actions.extend(ensures);

    Ok(ReconcilePlan {
        provider_id: provider_id.to_string(),
        topology_generation: topology.generation,
        provider_generation: desired.generation,
        report_sequence: report.sequence,
        actions,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{
        AllocationCommitRequest, AllocationLedgerSnapshot, CapacityHeartbeat,
        CapacityProviderStatus, ResourceAllocation,
    };
    use crate::topology::{
        Inventory, ProviderLocation, ResourceProvider, RESOURCE_MEMORY_BYTES, RESOURCE_VCPU_MILLIS,
    };

    fn cpu() -> ResourceClass {
        ResourceClass::from(RESOURCE_VCPU_MILLIS)
    }

    fn memory() -> ResourceClass {
        ResourceClass::from(RESOURCE_MEMORY_BYTES)
    }

    fn topology() -> TopologySnapshot {
        TopologySnapshot {
            generation: 7,
            providers: vec![ResourceProvider {
                id: "host-a".into(),
                parent_id: None,
                location: ProviderLocation {
                    provider: "test".into(),
                    region: "us-east".into(),
                    zone: Some("az-a".into()),
                    rack: Some("rack-a".into()),
                    host: Some("host-a".into()),
                },
                traits: BTreeSet::new(),
                inventory: BTreeMap::from([
                    (
                        cpu(),
                        Inventory {
                            total: 8_000,
                            reserved: 0,
                            min_unit: 100,
                            max_unit: 8_000,
                            step_size: 100,
                        },
                    ),
                    (
                        memory(),
                        Inventory {
                            total: 16_000,
                            reserved: 0,
                            min_unit: 100,
                            max_unit: 16_000,
                            step_size: 100,
                        },
                    ),
                ]),
                disabled: false,
            }],
        }
    }

    fn resources(cpu_amount: u64, memory_amount: u64) -> BTreeMap<ResourceClass, u64> {
        BTreeMap::from([(cpu(), cpu_amount), (memory(), memory_amount)])
    }

    fn allocation(id: &str, token: &str) -> ResourceAllocation {
        ResourceAllocation {
            allocation_id: id.into(),
            consumer_id: format!("consumer-{id}"),
            placement_token: token.into(),
            provider_id: "host-a".into(),
            resources: resources(1_000, 2_000),
            created_at_unix_ms: 900,
            expires_at_unix_ms: None,
        }
    }

    fn ledger_with(allocation: ResourceAllocation) -> AllocationLedgerSnapshot {
        let topology = topology();
        let mut ledger = AllocationLedgerSnapshot::default();
        ledger
            .commit(
                &topology,
                &AllocationCommitRequest {
                    expected_provider_generation: 1,
                    topology_generation: topology.generation,
                    allocation,
                },
            )
            .unwrap();
        ledger
    }

    fn ready_heartbeats(topology: &TopologySnapshot) -> CapacityHeartbeatBook {
        let mut heartbeats = CapacityHeartbeatBook::default();
        heartbeats
            .apply(
                topology,
                CapacityHeartbeat {
                    provider_id: "host-a".into(),
                    topology_generation: topology.generation,
                    sequence: 1,
                    observed_at_unix_ms: 1_000,
                    status: CapacityProviderStatus::Ready,
                    observed_in_use: BTreeMap::new(),
                },
            )
            .unwrap();
        heartbeats
    }

    fn observed(id: &str, token: &str) -> ObservedAllocation {
        ObservedAllocation {
            allocation_id: id.into(),
            placement_token: token.into(),
            resources: resources(1_000, 2_000),
        }
    }

    fn report(active_allocations: Vec<ObservedAllocation>) -> ProviderAllocationReport {
        ProviderAllocationReport {
            provider_id: "host-a".into(),
            topology_generation: 7,
            sequence: 1,
            observed_at_unix_ms: 1_000,
            active_allocations,
        }
    }

    fn report_book(
        topology: &TopologySnapshot,
        report: ProviderAllocationReport,
    ) -> AllocationReportBook {
        let mut reports = AllocationReportBook::default();
        reports.apply(topology, report).unwrap();
        reports
    }

    #[test]
    fn report_is_topology_bound_and_monotonic() {
        let topology = topology();
        let mut reports = AllocationReportBook::default();
        reports.apply(&topology, report(Vec::new())).unwrap();

        assert!(matches!(
            reports.apply(&topology, report(Vec::new())),
            Err(AllocationReportError::StaleSequence { .. })
        ));

        let mut stale = report(Vec::new());
        stale.topology_generation = 6;
        stale.sequence = 2;
        assert_eq!(
            reports.apply(&topology, stale),
            Err(AllocationReportError::StaleTopology {
                observed: 6,
                current: 7
            })
        );
    }

    #[test]
    fn clean_provider_needs_no_actions() {
        let topology = topology();
        let ledger = ledger_with(allocation("alloc-1", "token-1"));
        let heartbeats = ready_heartbeats(&topology);
        let reports = report_book(&topology, report(vec![observed("alloc-1", "token-1")]));

        let plan = reconcile_provider(
            &topology,
            &ledger,
            &heartbeats,
            &reports,
            "host-a",
            1_000,
            100,
            100,
        )
        .unwrap();

        assert!(plan.is_clean());
        assert_eq!(plan.provider_generation, 2);
    }

    #[test]
    fn missing_desired_allocation_is_ensured() {
        let topology = topology();
        let ledger = ledger_with(allocation("alloc-1", "token-1"));
        let reports = report_book(&topology, report(Vec::new()));

        let plan = reconcile_provider(
            &topology,
            &ledger,
            &ready_heartbeats(&topology),
            &reports,
            "host-a",
            1_000,
            100,
            100,
        )
        .unwrap();

        assert!(matches!(
            &plan.actions[..],
            [ReconcileAction::EnsureDesired { allocation_id, .. }] if allocation_id == "alloc-1"
        ));
    }

    #[test]
    fn orphan_and_retired_allocations_are_stopped() {
        let topology = topology();
        let mut ledger = ledger_with(allocation("retired", "desired-token"));
        ledger.release("host-a", "retired", 2).unwrap();

        let reports = report_book(
            &topology,
            report(vec![
                observed("orphan", "orphan-token"),
                observed("retired", "old-token"),
            ]),
        );

        let plan = reconcile_provider(
            &topology,
            &ledger,
            &ready_heartbeats(&topology),
            &reports,
            "host-a",
            1_000,
            100,
            100,
        )
        .unwrap();

        assert_eq!(plan.actions.len(), 2);
        assert!(matches!(
            &plan.actions[0],
            ReconcileAction::StopUnexpected {
                allocation_id,
                reason: StopReason::Orphan,
                ..
            } if allocation_id == "orphan"
        ));
        assert!(matches!(
            &plan.actions[1],
            ReconcileAction::StopUnexpected {
                allocation_id,
                reason: StopReason::Retired,
                ..
            } if allocation_id == "retired"
        ));
    }

    #[test]
    fn token_mismatch_stops_stale_instance_before_ensuring_desired() {
        let topology = topology();
        let ledger = ledger_with(allocation("alloc-1", "desired-token"));
        let reports = report_book(
            &topology,
            report(vec![observed("alloc-1", "stale-token")]),
        );

        let plan = reconcile_provider(
            &topology,
            &ledger,
            &ready_heartbeats(&topology),
            &reports,
            "host-a",
            1_000,
            100,
            100,
        )
        .unwrap();

        assert_eq!(plan.actions.len(), 2);
        assert!(matches!(
            &plan.actions[0],
            ReconcileAction::StopUnexpected {
                reason: StopReason::PlacementTokenMismatch,
                ..
            }
        ));
        assert!(matches!(
            &plan.actions[1],
            ReconcileAction::EnsureDesired { placement_token, .. }
                if placement_token == "desired-token"
        ));
    }

    #[test]
    fn resource_mismatch_is_verified_not_automatically_restarted() {
        let topology = topology();
        let ledger = ledger_with(allocation("alloc-1", "token-1"));
        let mut wrong = observed("alloc-1", "token-1");
        wrong.resources = resources(2_000, 2_000);
        let reports = report_book(&topology, report(vec![wrong]));

        let plan = reconcile_provider(
            &topology,
            &ledger,
            &ready_heartbeats(&topology),
            &reports,
            "host-a",
            1_000,
            100,
            100,
        )
        .unwrap();

        assert!(matches!(
            &plan.actions[..],
            [ReconcileAction::VerifyResourceDrift { allocation_id, .. }]
                if allocation_id == "alloc-1"
        ));
    }

    #[test]
    fn stale_report_or_heartbeat_fails_closed() {
        let topology = topology();
        let ledger = ledger_with(allocation("alloc-1", "token-1"));
        let reports = report_book(&topology, report(Vec::new()));

        assert_eq!(
            reconcile_provider(
                &topology,
                &ledger,
                &ready_heartbeats(&topology),
                &reports,
                "host-a",
                1_201,
                100,
                500,
            ),
            Err(ReconcileError::ProviderNotReady("host-a".into()))
        );

        assert_eq!(
            reconcile_provider(
                &topology,
                &ledger,
                &ready_heartbeats(&topology),
                &reports,
                "host-a",
                1_201,
                500,
                100,
            ),
            Err(ReconcileError::MissingOrStaleReport("host-a".into()))
        );
    }

    #[test]
    fn future_report_fails_closed() {
        let topology = topology();
        let ledger = ledger_with(allocation("alloc-1", "token-1"));
        let mut future_report = report(Vec::new());
        future_report.observed_at_unix_ms = 1_001;
        let reports = report_book(&topology, future_report);

        assert_eq!(
            reconcile_provider(
                &topology,
                &ledger,
                &ready_heartbeats(&topology),
                &reports,
                "host-a",
                1_000,
                100,
                100,
            ),
            Err(ReconcileError::MissingOrStaleReport("host-a".into()))
        );
    }
}
