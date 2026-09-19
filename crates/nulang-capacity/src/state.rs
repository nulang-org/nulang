//! Live capacity heartbeats and durable allocation-ledger contracts.
//!
//! Heartbeats answer "is this concrete provider currently schedulable?".
//! The allocation ledger answers "what capacity has the control plane already
//! promised?". These are intentionally separate sources of truth: observed
//! runtime usage may be used for drift detection, but it never silently replaces
//! durable reservation accounting.

use crate::topology::{
    allocation_candidates, AllocationCandidate, AllocationRequest, ProviderUsage, ResourceClass,
    TopologyError, TopologySnapshot,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::pin::Pin;
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapacityProviderStatus {
    Ready,
    Draining,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapacityHeartbeat {
    pub provider_id: String,
    /// Fleet topology generation against which this observation was produced.
    pub topology_generation: u64,
    /// Monotonic per-provider sequence within one topology generation.
    pub sequence: u64,
    pub observed_at_unix_ms: u64,
    pub status: CapacityProviderStatus,
    /// Runtime-observed use. This is reconciliation evidence only; placement
    /// capacity is derived from the durable allocation ledger.
    pub observed_in_use: BTreeMap<ResourceClass, u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapacityHeartbeatBook {
    latest: BTreeMap<String, CapacityHeartbeat>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum HeartbeatError {
    #[error("heartbeat provider id must not be empty")]
    EmptyProvider,
    #[error("heartbeat references unknown provider {0}")]
    UnknownProvider(String),
    #[error(
        "heartbeat topology generation {observed} does not match current generation {current}"
    )]
    StaleTopology { observed: u64, current: u64 },
    #[error(
        "heartbeat sequence {observed} is not newer than current sequence {current} for provider {provider}"
    )]
    StaleSequence {
        provider: String,
        observed: u64,
        current: u64,
    },
    #[error("heartbeat reports unknown resource class {resource} on provider {provider}")]
    UnknownResource {
        provider: String,
        resource: ResourceClass,
    },
    #[error(transparent)]
    Topology(#[from] TopologyError),
}

impl CapacityHeartbeatBook {
    pub fn apply(
        &mut self,
        topology: &TopologySnapshot,
        heartbeat: CapacityHeartbeat,
    ) -> Result<(), HeartbeatError> {
        topology.validate()?;

        if heartbeat.provider_id.trim().is_empty() {
            return Err(HeartbeatError::EmptyProvider);
        }
        if heartbeat.topology_generation != topology.generation {
            return Err(HeartbeatError::StaleTopology {
                observed: heartbeat.topology_generation,
                current: topology.generation,
            });
        }

        let provider = topology
            .provider(&heartbeat.provider_id)
            .ok_or_else(|| HeartbeatError::UnknownProvider(heartbeat.provider_id.clone()))?;

        for resource in heartbeat.observed_in_use.keys() {
            if !provider.inventory.contains_key(resource) {
                return Err(HeartbeatError::UnknownResource {
                    provider: heartbeat.provider_id.clone(),
                    resource: resource.clone(),
                });
            }
        }

        if let Some(current) = self.latest.get(&heartbeat.provider_id) {
            if current.topology_generation == heartbeat.topology_generation
                && heartbeat.sequence <= current.sequence
            {
                return Err(HeartbeatError::StaleSequence {
                    provider: heartbeat.provider_id.clone(),
                    observed: heartbeat.sequence,
                    current: current.sequence,
                });
            }
        }

        self.latest
            .insert(heartbeat.provider_id.clone(), heartbeat);
        Ok(())
    }

    pub fn latest(&self, provider_id: &str) -> Option<&CapacityHeartbeat> {
        self.latest.get(provider_id)
    }

    pub fn is_schedulable(
        &self,
        provider_id: &str,
        topology_generation: u64,
        now_unix_ms: u64,
        max_age_ms: u64,
    ) -> bool {
        self.latest.get(provider_id).is_some_and(|heartbeat| {
            heartbeat.topology_generation == topology_generation
                && heartbeat.status == CapacityProviderStatus::Ready
                && now_unix_ms.saturating_sub(heartbeat.observed_at_unix_ms) <= max_age_ms
        })
    }
}

/// Generate hard candidates using durable ledger usage, then fail closed on
/// missing/stale/draining heartbeats.
pub fn live_allocation_candidates<'a>(
    topology: &'a TopologySnapshot,
    ledger: &AllocationLedgerSnapshot,
    heartbeats: &CapacityHeartbeatBook,
    request: &AllocationRequest,
    now_unix_ms: u64,
    max_heartbeat_age_ms: u64,
) -> Result<Vec<AllocationCandidate<'a>>, CapacityStateError> {
    let usage = ledger.usage()?;
    let mut candidates = allocation_candidates(topology, &usage, request)?;
    candidates.retain(|candidate| {
        heartbeats.is_schedulable(
            &candidate.provider.id,
            topology.generation,
            now_unix_ms,
            max_heartbeat_age_ms,
        )
    });
    Ok(candidates)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceAllocation {
    pub allocation_id: String,
    pub consumer_id: String,
    pub placement_token: String,
    pub provider_id: String,
    pub resources: BTreeMap<ResourceClass, u64>,
    pub created_at_unix_ms: u64,
    pub expires_at_unix_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AllocationLedgerSnapshot {
    pub provider_generations: BTreeMap<String, u64>,
    pub allocations: BTreeMap<String, ResourceAllocation>,
}

impl Default for AllocationLedgerSnapshot {
    fn default() -> Self {
        Self {
            provider_generations: BTreeMap::new(),
            allocations: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllocationCommitRequest {
    pub expected_provider_generation: u64,
    pub topology_generation: u64,
    pub allocation: ResourceAllocation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AllocationCommitResult {
    Committed { generation: u64 },
    AlreadyCommitted { generation: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AllocationReleaseResult {
    Released { generation: u64 },
    AlreadyAbsent { generation: u64 },
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum AllocationError {
    #[error("allocation id must not be empty")]
    EmptyAllocationId,
    #[error("allocation consumer id must not be empty")]
    EmptyConsumerId,
    #[error("allocation placement token must not be empty")]
    EmptyPlacementToken,
    #[error("allocation provider id must not be empty")]
    EmptyProviderId,
    #[error("allocation must contain at least one resource")]
    EmptyResources,
    #[error("allocation request for {0} must be greater than zero")]
    ZeroResource(ResourceClass),
    #[error("allocation id {0} is already committed with different contents")]
    AllocationIdConflict(String),
    #[error(
        "consumer {consumer} placement token {placement_token} is already bound to allocation {existing_allocation_id}"
    )]
    PlacementTokenConflict {
        consumer: String,
        placement_token: String,
        existing_allocation_id: String,
    },
    #[error("provider {0} does not exist in the current topology")]
    UnknownProvider(String),
    #[error("provider {0} is disabled")]
    ProviderDisabled(String),
    #[error(
        "allocation topology generation {observed} does not match current generation {current}"
    )]
    StaleTopology { observed: u64, current: u64 },
    #[error(
        "allocation generation for provider {provider} expected {expected}, current {current}"
    )]
    GenerationMismatch {
        provider: String,
        expected: u64,
        current: u64,
    },
    #[error(
        "allocation {allocation_id} belongs to provider {actual_provider}, not {requested_provider}"
    )]
    AllocationProviderMismatch {
        allocation_id: String,
        requested_provider: String,
        actual_provider: String,
    },
    #[error("provider {provider} has no inventory for resource {resource}")]
    MissingInventory {
        provider: String,
        resource: ResourceClass,
    },
    #[error(
        "allocation amount {requested} for {resource} is not valid for provider {provider}"
    )]
    InvalidAllocationUnit {
        provider: String,
        resource: ResourceClass,
        requested: u64,
    },
    #[error(
        "provider {provider} lacks {resource}: requested {requested}, available {available}"
    )]
    InsufficientCapacity {
        provider: String,
        resource: ResourceClass,
        requested: u64,
        available: u64,
    },
    #[error("resource usage overflow for {resource} on provider {provider}")]
    UsageOverflow {
        provider: String,
        resource: ResourceClass,
    },
    #[error(transparent)]
    Topology(#[from] TopologyError),
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum CapacityStateError {
    #[error(transparent)]
    Allocation(#[from] AllocationError),
    #[error(transparent)]
    Topology(#[from] TopologyError),
}

impl AllocationLedgerSnapshot {
    pub fn provider_generation(&self, provider_id: &str) -> u64 {
        self.provider_generations
            .get(provider_id)
            .copied()
            .unwrap_or(1)
    }

    fn advance_provider_generation(&mut self, provider_id: &str) -> u64 {
        let next = self.provider_generation(provider_id).saturating_add(1);
        self.provider_generations.insert(provider_id.to_string(), next);
        next
    }

    pub fn provider_snapshot(&self, provider_id: &str) -> ProviderAllocationLedgerSnapshot {
        ProviderAllocationLedgerSnapshot {
            provider_id: provider_id.to_string(),
            generation: self.provider_generation(provider_id),
            allocations: self
                .allocations
                .iter()
                .filter(|(_, allocation)| allocation.provider_id == provider_id)
                .map(|(id, allocation)| (id.clone(), allocation.clone()))
                .collect(),
        }
    }

    pub fn usage(&self) -> Result<BTreeMap<String, ProviderUsage>, AllocationError> {
        let mut usage: BTreeMap<String, ProviderUsage> = BTreeMap::new();
        for allocation in self.allocations.values() {
            let provider_usage = usage.entry(allocation.provider_id.clone()).or_default();
            for (resource, amount) in &allocation.resources {
                let current = provider_usage
                    .allocated
                    .get(resource)
                    .copied()
                    .unwrap_or(0);
                let next = current
                    .checked_add(*amount)
                    .ok_or_else(|| AllocationError::UsageOverflow {
                        provider: allocation.provider_id.clone(),
                        resource: resource.clone(),
                    })?;
                provider_usage.allocated.insert(resource.clone(), next);
            }
        }
        Ok(usage)
    }

    /// Mutate an in-memory ledger snapshot using the exact rules a durable CAS
    /// store must preserve. Callers persist the resulting snapshot atomically.
    pub fn commit(
        &mut self,
        topology: &TopologySnapshot,
        request: &AllocationCommitRequest,
    ) -> Result<AllocationCommitResult, AllocationError> {
        topology.validate()?;
        validate_allocation(&request.allocation)?;

        if let Some(existing) = self.allocations.get(&request.allocation.allocation_id) {
            if existing == &request.allocation {
                return Ok(AllocationCommitResult::AlreadyCommitted {
                    generation: self.provider_generation(&existing.provider_id),
                });
            }
            return Err(AllocationError::AllocationIdConflict(
                request.allocation.allocation_id.clone(),
            ));
        }

        if let Some(existing) = self.allocations.values().find(|existing| {
            existing.consumer_id == request.allocation.consumer_id
                && existing.placement_token == request.allocation.placement_token
        }) {
            return Err(AllocationError::PlacementTokenConflict {
                consumer: request.allocation.consumer_id.clone(),
                placement_token: request.allocation.placement_token.clone(),
                existing_allocation_id: existing.allocation_id.clone(),
            });
        }

        if request.topology_generation != topology.generation {
            return Err(AllocationError::StaleTopology {
                observed: request.topology_generation,
                current: topology.generation,
            });
        }
        let current_generation = self.provider_generation(&request.allocation.provider_id);
        if request.expected_provider_generation != current_generation {
            return Err(AllocationError::GenerationMismatch {
                provider: request.allocation.provider_id.clone(),
                expected: request.expected_provider_generation,
                current: current_generation,
            });
        }

        let provider = topology
            .provider(&request.allocation.provider_id)
            .ok_or_else(|| AllocationError::UnknownProvider(request.allocation.provider_id.clone()))?;
        if provider.disabled {
            return Err(AllocationError::ProviderDisabled(provider.id.clone()));
        }

        let usage = self.usage()?;
        let provider_usage = usage.get(&provider.id);

        for (resource, requested) in &request.allocation.resources {
            let inventory = provider.inventory.get(resource).ok_or_else(|| {
                AllocationError::MissingInventory {
                    provider: provider.id.clone(),
                    resource: resource.clone(),
                }
            })?;
            if !inventory.accepts(*requested) {
                return Err(AllocationError::InvalidAllocationUnit {
                    provider: provider.id.clone(),
                    resource: resource.clone(),
                    requested: *requested,
                });
            }

            let already_allocated = provider_usage
                .and_then(|value| value.allocated.get(resource))
                .copied()
                .unwrap_or(0);
            let available = inventory.usable().saturating_sub(already_allocated);
            if *requested > available {
                return Err(AllocationError::InsufficientCapacity {
                    provider: provider.id.clone(),
                    resource: resource.clone(),
                    requested: *requested,
                    available,
                });
            }
        }

        self.allocations.insert(
            request.allocation.allocation_id.clone(),
            request.allocation.clone(),
        );
        let generation = self.advance_provider_generation(&provider.id);
        Ok(AllocationCommitResult::Committed { generation })
    }

    pub fn release(
        &mut self,
        provider_id: &str,
        allocation_id: &str,
        expected_provider_generation: u64,
    ) -> Result<AllocationReleaseResult, AllocationError> {
        let current_generation = self.provider_generation(provider_id);
        let Some(existing) = self.allocations.get(allocation_id) else {
            return Ok(AllocationReleaseResult::AlreadyAbsent {
                generation: current_generation,
            });
        };
        if existing.provider_id != provider_id {
            return Err(AllocationError::AllocationProviderMismatch {
                allocation_id: allocation_id.to_string(),
                requested_provider: provider_id.to_string(),
                actual_provider: existing.provider_id.clone(),
            });
        }
        if expected_provider_generation != current_generation {
            return Err(AllocationError::GenerationMismatch {
                provider: provider_id.to_string(),
                expected: expected_provider_generation,
                current: current_generation,
            });
        }

        self.allocations.remove(allocation_id);
        let generation = self.advance_provider_generation(provider_id);
        Ok(AllocationReleaseResult::Released { generation })
    }

    pub fn expire_before(&mut self, now_unix_ms: u64) -> Vec<ResourceAllocation> {
        let expired_ids: Vec<String> = self
            .allocations
            .iter()
            .filter_map(|(id, allocation)| {
                allocation
                    .expires_at_unix_ms
                    .filter(|deadline| *deadline <= now_unix_ms)
                    .map(|_| id.clone())
            })
            .collect();

        let mut expired = Vec::with_capacity(expired_ids.len());
        let mut affected_providers = BTreeSet::new();
        for id in expired_ids {
            if let Some(allocation) = self.allocations.remove(&id) {
                affected_providers.insert(allocation.provider_id.clone());
                expired.push(allocation);
            }
        }
        for provider_id in affected_providers {
            self.advance_provider_generation(&provider_id);
        }
        expired
    }
}

fn validate_allocation(allocation: &ResourceAllocation) -> Result<(), AllocationError> {
    if allocation.allocation_id.trim().is_empty() {
        return Err(AllocationError::EmptyAllocationId);
    }
    if allocation.consumer_id.trim().is_empty() {
        return Err(AllocationError::EmptyConsumerId);
    }
    if allocation.placement_token.trim().is_empty() {
        return Err(AllocationError::EmptyPlacementToken);
    }
    if allocation.provider_id.trim().is_empty() {
        return Err(AllocationError::EmptyProviderId);
    }
    if allocation.resources.is_empty() {
        return Err(AllocationError::EmptyResources);
    }
    for (resource, amount) in &allocation.resources {
        if *amount == 0 {
            return Err(AllocationError::ZeroResource(resource.clone()));
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapacityDrift {
    pub resource: ResourceClass,
    pub ledger_allocated: u64,
    pub observed_in_use: u64,
}

/// Aggregate reconciliation evidence. A difference is a signal to investigate,
/// not permission to rewrite the durable allocation ledger from a heartbeat.
pub fn usage_drift(
    provider_id: &str,
    ledger: &AllocationLedgerSnapshot,
    heartbeats: &CapacityHeartbeatBook,
) -> Result<Vec<CapacityDrift>, AllocationError> {
    let usage = ledger.usage()?;
    let ledger_usage = usage.get(provider_id);
    let heartbeat = heartbeats.latest(provider_id);

    let mut resources = BTreeSet::new();
    if let Some(current) = ledger_usage {
        resources.extend(current.allocated.keys().cloned());
    }
    if let Some(current) = heartbeat {
        resources.extend(current.observed_in_use.keys().cloned());
    }

    let mut drift = Vec::new();
    for resource in resources {
        let ledger_allocated = ledger_usage
            .and_then(|current| current.allocated.get(&resource))
            .copied()
            .unwrap_or(0);
        let observed_in_use = heartbeat
            .and_then(|current| current.observed_in_use.get(&resource))
            .copied()
            .unwrap_or(0);
        if ledger_allocated != observed_in_use {
            drift.push(CapacityDrift {
                resource,
                ledger_allocated,
                observed_in_use,
            });
        }
    }
    Ok(drift)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderAllocationLedgerSnapshot {
    pub provider_id: String,
    pub generation: u64,
    pub allocations: BTreeMap<String, ResourceAllocation>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LedgerCasResult {
    Stored,
    GenerationChanged,
}

pub type LedgerLoadFuture<'a> = Pin<
    Box<dyn Future<Output = Result<ProviderAllocationLedgerSnapshot, String>> + Send + 'a>,
>;
pub type LedgerCasFuture<'a> =
    Pin<Box<dyn Future<Output = Result<LedgerCasResult, String>> + Send + 'a>>;

/// Durable provider-scoped storage boundary for the allocation ledger.
///
/// Provider-local generations avoid a global scheduling lock: a reservation on
/// one host does not force retries for unrelated hosts. A production store may
/// use normalized rows rather than serialized snapshots, but compare_and_swap
/// must atomically enforce the provider generation and allocation identity.
pub trait AllocationLedgerStore: Send + Sync {
    fn load_provider<'a>(&'a self, provider_id: &'a str) -> LedgerLoadFuture<'a>;

    fn compare_and_swap_provider<'a>(
        &'a self,
        expected_generation: u64,
        next: &'a ProviderAllocationLedgerSnapshot,
    ) -> LedgerCasFuture<'a>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::topology::{Inventory, ProviderLocation, ResourceProvider};

    fn cpu() -> ResourceClass {
        ResourceClass::from("VCPU_MILLIS")
    }

    fn topology() -> TopologySnapshot {
        TopologySnapshot {
            generation: 9,
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
                traits: BTreeSet::from(["RUNTIME_NULANG".into()]),
                inventory: BTreeMap::from([(
                    cpu(),
                    Inventory {
                        total: 4_000,
                        reserved: 500,
                        min_unit: 100,
                        max_unit: 3_500,
                        step_size: 100,
                    },
                )]),
                disabled: false,
            }],
        }
    }

    fn heartbeat(sequence: u64, status: CapacityProviderStatus, observed: u64) -> CapacityHeartbeat {
        CapacityHeartbeat {
            provider_id: "host-a".into(),
            topology_generation: 9,
            sequence,
            observed_at_unix_ms: 1_000,
            status,
            observed_in_use: BTreeMap::from([(cpu(), observed)]),
        }
    }

    fn allocation(id: &str, amount: u64) -> ResourceAllocation {
        ResourceAllocation {
            allocation_id: id.into(),
            consumer_id: "actor:orders:42".into(),
            placement_token: format!("token-{id}"),
            provider_id: "host-a".into(),
            resources: BTreeMap::from([(cpu(), amount)]),
            created_at_unix_ms: 1_000,
            expires_at_unix_ms: None,
        }
    }

    #[test]
    fn heartbeat_is_topology_bound_and_monotonic() {
        let topology = topology();
        let mut book = CapacityHeartbeatBook::default();
        book.apply(
            &topology,
            heartbeat(7, CapacityProviderStatus::Ready, 0),
        )
        .unwrap();

        assert!(matches!(
            book.apply(
                &topology,
                heartbeat(7, CapacityProviderStatus::Ready, 0)
            ),
            Err(HeartbeatError::StaleSequence { .. })
        ));

        let mut stale = heartbeat(8, CapacityProviderStatus::Ready, 0);
        stale.topology_generation = 8;
        assert_eq!(
            book.apply(&topology, stale),
            Err(HeartbeatError::StaleTopology {
                observed: 8,
                current: 9
            })
        );
    }

    #[test]
    fn heartbeat_sequence_can_restart_after_topology_generation_changes() {
        let mut topology = topology();
        let mut book = CapacityHeartbeatBook::default();
        book.apply(
            &topology,
            heartbeat(99, CapacityProviderStatus::Ready, 0),
        )
        .unwrap();

        topology.generation = 10;
        let mut next_generation = heartbeat(1, CapacityProviderStatus::Ready, 0);
        next_generation.topology_generation = 10;
        book.apply(&topology, next_generation).unwrap();

        assert!(book.is_schedulable("host-a", 10, 1_000, 50));
        assert!(!book.is_schedulable("host-a", 9, 1_000, 50));
    }

    #[test]
    fn live_candidates_fail_closed_on_draining_or_stale_provider() {
        let topology = topology();
        let ledger = AllocationLedgerSnapshot::default();
        let request = AllocationRequest {
            resources: BTreeMap::from([(cpu(), 100)]),
            ..AllocationRequest::default()
        };
        let mut book = CapacityHeartbeatBook::default();

        book.apply(
            &topology,
            heartbeat(1, CapacityProviderStatus::Draining, 0),
        )
        .unwrap();
        assert!(
            live_allocation_candidates(&topology, &ledger, &book, &request, 1_000, 50)
                .unwrap()
                .is_empty()
        );

        book.apply(
            &topology,
            heartbeat(2, CapacityProviderStatus::Ready, 0),
        )
        .unwrap();
        assert_eq!(
            live_allocation_candidates(&topology, &ledger, &book, &request, 1_049, 50)
                .unwrap()
                .len(),
            1
        );
        assert!(
            live_allocation_candidates(&topology, &ledger, &book, &request, 1_051, 50)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn allocation_commit_is_idempotent_and_generation_fenced() {
        let topology = topology();
        let mut ledger = AllocationLedgerSnapshot::default();
        let first = allocation("alloc-1", 2_000);
        let request = AllocationCommitRequest {
            expected_provider_generation: 1,
            topology_generation: 9,
            allocation: first.clone(),
        };

        assert_eq!(
            ledger.commit(&topology, &request).unwrap(),
            AllocationCommitResult::Committed { generation: 2 }
        );
        assert_eq!(
            ledger.commit(&topology, &request).unwrap(),
            AllocationCommitResult::AlreadyCommitted { generation: 2 }
        );

        let stale_request = AllocationCommitRequest {
            expected_provider_generation: 1,
            topology_generation: 9,
            allocation: allocation("alloc-2", 1_000),
        };
        assert_eq!(
            ledger.commit(&topology, &stale_request),
            Err(AllocationError::GenerationMismatch {
                provider: "host-a".into(),
                expected: 1,
                current: 2
            })
        );
    }

    #[test]
    fn provider_generations_do_not_conflict_across_unrelated_hosts() {
        let mut topology = topology();
        let mut host_b = topology.providers[0].clone();
        host_b.id = "host-b".into();
        host_b.location.host = Some("host-b".into());
        host_b.location.zone = Some("az-b".into());
        topology.providers.push(host_b);

        let mut ledger = AllocationLedgerSnapshot::default();
        ledger
            .commit(
                &topology,
                &AllocationCommitRequest {
                    expected_provider_generation: 1,
                    topology_generation: 9,
                    allocation: allocation("alloc-a", 500),
                },
            )
            .unwrap();

        let mut on_b = allocation("alloc-b", 500);
        on_b.provider_id = "host-b".into();
        on_b.placement_token = "token-b".into();
        ledger
            .commit(
                &topology,
                &AllocationCommitRequest {
                    expected_provider_generation: 1,
                    topology_generation: 9,
                    allocation: on_b,
                },
            )
            .unwrap();

        assert_eq!(ledger.provider_generation("host-a"), 2);
        assert_eq!(ledger.provider_generation("host-b"), 2);
    }

    #[test]
    fn placement_token_cannot_reserve_twice_under_different_allocation_ids() {
        let topology = topology();
        let mut ledger = AllocationLedgerSnapshot::default();
        ledger
            .commit(
                &topology,
                &AllocationCommitRequest {
                    expected_provider_generation: 1,
                    topology_generation: 9,
                    allocation: allocation("alloc-1", 500),
                },
            )
            .unwrap();

        let mut duplicate = allocation("alloc-2", 500);
        duplicate.placement_token = "token-alloc-1".into();
        assert_eq!(
            ledger.commit(
                &topology,
                &AllocationCommitRequest {
                    expected_provider_generation: 2,
                    topology_generation: 9,
                    allocation: duplicate,
                }
            ),
            Err(AllocationError::PlacementTokenConflict {
                consumer: "actor:orders:42".into(),
                placement_token: "token-alloc-1".into(),
                existing_allocation_id: "alloc-1".into()
            })
        );
    }

    #[test]
    fn ledger_prevents_overcommit_after_retry_on_fresh_generation() {
        let topology = topology();
        let mut ledger = AllocationLedgerSnapshot::default();
        ledger
            .commit(
                &topology,
                &AllocationCommitRequest {
                    expected_provider_generation: 1,
                    topology_generation: 9,
                    allocation: allocation("alloc-1", 2_500),
                },
            )
            .unwrap();

        assert_eq!(
            ledger.commit(
                &topology,
                &AllocationCommitRequest {
                    expected_provider_generation: 2,
                    topology_generation: 9,
                    allocation: allocation("alloc-2", 1_100),
                }
            ),
            Err(AllocationError::InsufficientCapacity {
                provider: "host-a".into(),
                resource: cpu(),
                requested: 1_100,
                available: 1_000
            })
        );
    }

    #[test]
    fn release_is_idempotent_and_frees_capacity() {
        let topology = topology();
        let mut ledger = AllocationLedgerSnapshot::default();
        ledger
            .commit(
                &topology,
                &AllocationCommitRequest {
                    expected_provider_generation: 1,
                    topology_generation: 9,
                    allocation: allocation("alloc-1", 3_000),
                },
            )
            .unwrap();

        assert_eq!(
            ledger.release("host-a", "alloc-1", 2).unwrap(),
            AllocationReleaseResult::Released { generation: 3 }
        );
        assert_eq!(
            ledger.release("host-a", "alloc-1", 1).unwrap(),
            AllocationReleaseResult::AlreadyAbsent { generation: 3 }
        );

        ledger
            .commit(
                &topology,
                &AllocationCommitRequest {
                    expected_provider_generation: 3,
                    topology_generation: 9,
                    allocation: allocation("alloc-2", 3_000),
                },
            )
            .unwrap();
    }

    #[test]
    fn heartbeat_usage_is_drift_evidence_not_allocation_authority() {
        let topology = topology();
        let mut ledger = AllocationLedgerSnapshot::default();
        ledger
            .commit(
                &topology,
                &AllocationCommitRequest {
                    expected_provider_generation: 1,
                    topology_generation: 9,
                    allocation: allocation("alloc-1", 1_500),
                },
            )
            .unwrap();

        let mut book = CapacityHeartbeatBook::default();
        book.apply(
            &topology,
            heartbeat(1, CapacityProviderStatus::Ready, 2_000),
        )
        .unwrap();

        let drift = usage_drift("host-a", &ledger, &book).unwrap();
        assert_eq!(
            drift,
            vec![CapacityDrift {
                resource: cpu(),
                ledger_allocated: 1_500,
                observed_in_use: 2_000
            }]
        );

        let request = AllocationRequest {
            resources: BTreeMap::from([(cpu(), 2_000)]),
            ..AllocationRequest::default()
        };
        assert_eq!(
            live_allocation_candidates(&topology, &ledger, &book, &request, 1_000, 100)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn expiry_reclaims_records_with_one_generation_advance() {
        let topology = topology();
        let mut ledger = AllocationLedgerSnapshot::default();
        let mut expiring = allocation("alloc-1", 500);
        expiring.expires_at_unix_ms = Some(1_100);
        ledger
            .commit(
                &topology,
                &AllocationCommitRequest {
                    expected_provider_generation: 1,
                    topology_generation: 9,
                    allocation: expiring,
                },
            )
            .unwrap();

        assert!(ledger.expire_before(1_099).is_empty());
        let expired = ledger.expire_before(1_100);
        assert_eq!(expired.len(), 1);
        assert_eq!(ledger.provider_generation("host-a"), 3);
        assert!(ledger.allocations.is_empty());
    }
}
