//! Concrete resource-provider topology and deterministic failure-domain placement.
//!
//! Provider adapters and workers publish allocatable resource providers. The
//! control plane computes hard-constraint allocation candidates here before
//! applying economic/locality scoring. This mirrors OpenStack Placement's
//! candidate/scoring split while borrowing Ceph's topology-aware deterministic
//! placement principle without exposing Ceph placement groups to users.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt;
use thiserror::Error;

pub const RESOURCE_VCPU_MILLIS: &str = "VCPU_MILLIS";
pub const RESOURCE_MEMORY_BYTES: &str = "MEMORY_BYTES";
pub const RESOURCE_LOCAL_DISK_BYTES: &str = "LOCAL_DISK_BYTES";
pub const RESOURCE_NETWORK_MBPS: &str = "NETWORK_MBPS";
pub const RESOURCE_IOPS: &str = "IOPS";
pub const RESOURCE_ACCELERATOR_COUNT: &str = "ACCELERATOR_COUNT";
pub const RESOURCE_ACCELERATOR_MEMORY_BYTES: &str = "ACCELERATOR_MEMORY_BYTES";

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ResourceClass(pub String);

impl ResourceClass {
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }
}

impl From<&str> for ResourceClass {
    fn from(value: &str) -> Self {
        Self(value.to_string())
    }
}

impl fmt::Display for ResourceClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureDomain {
    Host,
    Rack,
    Zone,
    Region,
    Provider,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderLocation {
    pub provider: String,
    pub region: String,
    pub zone: Option<String>,
    pub rack: Option<String>,
    pub host: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct FailureDomainKey {
    pub provider: String,
    pub region: Option<String>,
    pub zone: Option<String>,
    pub rack: Option<String>,
    pub host: Option<String>,
}

impl ProviderLocation {
    /// Human-readable label for the requested level. This is not a globally
    /// unique failure-domain identity; use domain_key for placement decisions.
    pub fn domain_value(&self, domain: FailureDomain) -> Option<&str> {
        match domain {
            FailureDomain::Host => self.host.as_deref(),
            FailureDomain::Rack => self.rack.as_deref(),
            FailureDomain::Zone => self.zone.as_deref(),
            FailureDomain::Region => Some(self.region.as_str()),
            FailureDomain::Provider => Some(self.provider.as_str()),
        }
    }

    /// Canonical hierarchical identity for failure-domain comparisons.
    ///
    /// Cloud region/zone/rack/host names are provider-local. Including the
    /// parent scope prevents labels such as "us-east" or "az-a" from aliasing
    /// across providers or sibling domains.
    pub fn domain_key(&self, domain: FailureDomain) -> Option<FailureDomainKey> {
        match domain {
            FailureDomain::Provider => Some(FailureDomainKey {
                provider: self.provider.clone(),
                region: None,
                zone: None,
                rack: None,
                host: None,
            }),
            FailureDomain::Region => Some(FailureDomainKey {
                provider: self.provider.clone(),
                region: Some(self.region.clone()),
                zone: None,
                rack: None,
                host: None,
            }),
            FailureDomain::Zone => Some(FailureDomainKey {
                provider: self.provider.clone(),
                region: Some(self.region.clone()),
                zone: Some(self.zone.clone()?),
                rack: None,
                host: None,
            }),
            FailureDomain::Rack => Some(FailureDomainKey {
                provider: self.provider.clone(),
                region: Some(self.region.clone()),
                zone: self.zone.clone(),
                rack: Some(self.rack.clone()?),
                host: None,
            }),
            FailureDomain::Host => Some(FailureDomainKey {
                provider: self.provider.clone(),
                region: Some(self.region.clone()),
                zone: self.zone.clone(),
                rack: self.rack.clone(),
                host: Some(self.host.clone()?),
            }),
        }
    }
}

/// Integer capacity for one resource class. Units are defined by the class
/// (for example milli-vCPU or bytes), avoiding floating-point allocation drift.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Inventory {
    pub total: u64,
    pub reserved: u64,
    pub min_unit: u64,
    pub max_unit: u64,
    pub step_size: u64,
}

impl Inventory {
    pub fn usable(&self) -> u64 {
        self.total.saturating_sub(self.reserved)
    }

    pub fn accepts(&self, amount: u64) -> bool {
        amount >= self.min_unit
            && amount <= self.max_unit
            && self.step_size > 0
            && amount.is_multiple_of(self.step_size)
    }

    fn validate(&self) -> Result<(), InventoryError> {
        if self.reserved > self.total {
            return Err(InventoryError::ReservedExceedsTotal);
        }
        if self.min_unit == 0 {
            return Err(InventoryError::ZeroMinimum);
        }
        if self.step_size == 0 {
            return Err(InventoryError::ZeroStep);
        }
        if self.max_unit < self.min_unit || self.max_unit > self.usable() {
            return Err(InventoryError::InvalidMaximum);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum InventoryError {
    #[error("reserved capacity exceeds total capacity")]
    ReservedExceedsTotal,
    #[error("minimum allocation unit must be greater than zero")]
    ZeroMinimum,
    #[error("allocation step size must be greater than zero")]
    ZeroStep,
    #[error("maximum allocation unit must be >= minimum and <= usable capacity")]
    InvalidMaximum,
}

/// OpenStack-Placement-like resource provider. Providers may form a hierarchy,
/// such as rack -> host -> accelerator device. This first slice allocates from
/// one provider at a time; nested request groups can be added later.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceProvider {
    pub id: String,
    pub parent_id: Option<String>,
    pub location: ProviderLocation,
    pub traits: BTreeSet<String>,
    pub inventory: BTreeMap<ResourceClass, Inventory>,
    pub disabled: bool,
}

/// Immutable fleet view. Control-plane generations fence stale topology.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TopologySnapshot {
    pub generation: u64,
    pub providers: Vec<ResourceProvider>,
}

impl TopologySnapshot {
    pub fn validate(&self) -> Result<(), TopologyError> {
        let mut ids = HashSet::new();
        for provider in &self.providers {
            if provider.id.trim().is_empty() {
                return Err(TopologyError::EmptyProviderId);
            }
            if !ids.insert(provider.id.as_str()) {
                return Err(TopologyError::DuplicateProvider(provider.id.clone()));
            }
            for (resource, inventory) in &provider.inventory {
                if resource.0.trim().is_empty() {
                    return Err(TopologyError::EmptyResourceClass(provider.id.clone()));
                }
                inventory
                    .validate()
                    .map_err(|reason| TopologyError::InvalidInventory {
                        provider: provider.id.clone(),
                        resource: resource.clone(),
                        reason,
                    })?;
            }
        }

        let parents: HashMap<&str, Option<&str>> = self
            .providers
            .iter()
            .map(|provider| (provider.id.as_str(), provider.parent_id.as_deref()))
            .collect();

        for provider in &self.providers {
            if let Some(parent) = provider.parent_id.as_deref() {
                if !parents.contains_key(parent) {
                    return Err(TopologyError::MissingParent {
                        provider: provider.id.clone(),
                        parent: parent.to_string(),
                    });
                }
            }

            let mut seen = HashSet::new();
            let mut current = Some(provider.id.as_str());
            while let Some(id) = current {
                if !seen.insert(id) {
                    return Err(TopologyError::ParentCycle(provider.id.clone()));
                }
                current = *parents
                    .get(id)
                    .expect("provider and every validated parent must exist");
            }
        }
        Ok(())
    }

    pub fn provider(&self, id: &str) -> Option<&ResourceProvider> {
        self.providers.iter().find(|provider| provider.id == id)
    }

    pub fn children_of(&self, parent_id: &str) -> Vec<&ResourceProvider> {
        self.providers
            .iter()
            .filter(|provider| provider.parent_id.as_deref() == Some(parent_id))
            .collect()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderUsage {
    pub allocated: BTreeMap<ResourceClass, u64>,
}

/// Hard constraints only. Soft scoring belongs after candidate generation.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AllocationRequest {
    pub resources: BTreeMap<ResourceClass, u64>,
    pub required_traits: BTreeSet<String>,
    pub forbidden_traits: BTreeSet<String>,
    pub allowed_providers: BTreeSet<String>,
    pub allowed_regions: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllocationCandidate<'a> {
    pub provider: &'a ResourceProvider,
    /// Remaining capacity before this request is applied, for requested classes.
    pub free: BTreeMap<ResourceClass, u64>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum TopologyError {
    #[error("resource provider id must not be empty")]
    EmptyProviderId,
    #[error("resource provider {0} appears more than once")]
    DuplicateProvider(String),
    #[error("resource provider {0} has an empty resource class")]
    EmptyResourceClass(String),
    #[error("resource provider {provider} references missing parent {parent}")]
    MissingParent { provider: String, parent: String },
    #[error("resource provider hierarchy contains a cycle at {0}")]
    ParentCycle(String),
    #[error("invalid inventory {resource} on provider {provider}: {reason}")]
    InvalidInventory {
        provider: String,
        resource: ResourceClass,
        reason: InventoryError,
    },
    #[error("allocation request must contain at least one resource")]
    EmptyRequest,
    #[error("allocation request for {0} must be greater than zero")]
    ZeroRequest(ResourceClass),
    #[error("trait {0} cannot be both required and forbidden")]
    ConflictingTrait(String),
    #[error(
        "recorded usage for {resource} on provider {provider} ({allocated}) exceeds usable capacity ({usable})"
    )]
    UsageExceedsCapacity {
        provider: String,
        resource: ResourceClass,
        allocated: u64,
        usable: u64,
    },
    #[error(
        "cannot place {requested} replicas across distinct {domain:?} domains; only {available} eligible domains are available"
    )]
    InsufficientFailureDomains {
        requested: usize,
        available: usize,
        domain: FailureDomain,
    },
}

/// Return every provider that satisfies current hard constraints. The stable id
/// sort makes the candidate set reproducible but does not choose a winner.
pub fn allocation_candidates<'a>(
    topology: &'a TopologySnapshot,
    usage: &BTreeMap<String, ProviderUsage>,
    request: &AllocationRequest,
) -> Result<Vec<AllocationCandidate<'a>>, TopologyError> {
    topology.validate()?;
    validate_request(request)?;

    let mut candidates = Vec::new();
    for provider in &topology.providers {
        if provider.disabled {
            continue;
        }
        if !request.allowed_providers.is_empty()
            && !request
                .allowed_providers
                .contains(&provider.location.provider)
        {
            continue;
        }
        if !request.allowed_regions.is_empty()
            && !request.allowed_regions.contains(&provider.location.region)
        {
            continue;
        }
        if !request.required_traits.is_subset(&provider.traits)
            || request
                .forbidden_traits
                .iter()
                .any(|item| provider.traits.contains(item))
        {
            continue;
        }

        let provider_usage = usage.get(&provider.id);
        let mut free = BTreeMap::new();
        let mut eligible = true;

        for (resource, amount) in &request.resources {
            let Some(inventory) = provider.inventory.get(resource) else {
                eligible = false;
                break;
            };
            if !inventory.accepts(*amount) {
                eligible = false;
                break;
            }

            let allocated = provider_usage
                .and_then(|current| current.allocated.get(resource))
                .copied()
                .unwrap_or(0);
            let usable = inventory.usable();
            if allocated > usable {
                return Err(TopologyError::UsageExceedsCapacity {
                    provider: provider.id.clone(),
                    resource: resource.clone(),
                    allocated,
                    usable,
                });
            }
            let available = usable - allocated;
            if *amount > available {
                eligible = false;
                break;
            }
            free.insert(resource.clone(), available);
        }

        if eligible {
            candidates.push(AllocationCandidate { provider, free });
        }
    }

    candidates.sort_by(|left, right| left.provider.id.cmp(&right.provider.id));
    Ok(candidates)
}

fn validate_request(request: &AllocationRequest) -> Result<(), TopologyError> {
    if request.resources.is_empty() {
        return Err(TopologyError::EmptyRequest);
    }
    for (resource, amount) in &request.resources {
        if *amount == 0 {
            return Err(TopologyError::ZeroRequest(resource.clone()));
        }
    }
    if let Some(conflict) = request
        .required_traits
        .intersection(&request.forbidden_traits)
        .next()
    {
        return Err(TopologyError::ConflictingTrait(conflict.clone()));
    }
    Ok(())
}

/// Stable rendezvous-style provider ordering. Same key + same snapshot yields
/// the same order without a central per-object placement lookup table.
pub fn deterministic_provider_order<'a>(
    placement_key: &str,
    candidates: &[AllocationCandidate<'a>],
) -> Vec<&'a ResourceProvider> {
    let mut ordered: Vec<_> = candidates
        .iter()
        .map(|candidate| {
            (
                stable_placement_hash(placement_key, &candidate.provider.id),
                candidate.provider,
            )
        })
        .collect();
    ordered.sort_by(|(left_score, left), (right_score, right)| {
        right_score
            .cmp(left_score)
            .then_with(|| left.id.cmp(&right.id))
    });
    ordered.into_iter().map(|(_, provider)| provider).collect()
}

/// Deterministically select replicas in distinct requested failure domains.
pub fn select_spread_replicas<'a>(
    placement_key: &str,
    candidates: &[AllocationCandidate<'a>],
    replicas: usize,
    domain: FailureDomain,
) -> Result<Vec<&'a ResourceProvider>, TopologyError> {
    if replicas == 0 {
        return Ok(Vec::new());
    }

    let mut selected = Vec::with_capacity(replicas);
    let mut domains = BTreeSet::new();

    for provider in deterministic_provider_order(placement_key, candidates) {
        let Some(key) = provider.location.domain_key(domain) else {
            continue;
        };
        if domains.insert(key) {
            selected.push(provider);
            if selected.len() == replicas {
                return Ok(selected);
            }
        }
    }

    Err(TopologyError::InsufficientFailureDomains {
        requested: replicas,
        available: domains.len(),
        domain,
    })
}

/// Stable FNV-1a used only for deterministic placement ordering.
fn stable_placement_hash(placement_key: &str, provider_id: &str) -> u64 {
    const OFFSET: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x100000001b3;

    let mut hash = OFFSET;
    for byte in placement_key
        .as_bytes()
        .iter()
        .copied()
        .chain(std::iter::once(0xff))
        .chain(provider_id.as_bytes().iter().copied())
    {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cpu() -> ResourceClass {
        ResourceClass::from(RESOURCE_VCPU_MILLIS)
    }

    fn host(id: &str, zone: &str, rack: &str, traits: &[&str]) -> ResourceProvider {
        ResourceProvider {
            id: id.to_string(),
            parent_id: None,
            location: ProviderLocation {
                provider: "test-cloud".into(),
                region: "us-east".into(),
                zone: Some(zone.into()),
                rack: Some(rack.into()),
                host: Some(id.into()),
            },
            traits: traits.iter().map(|item| (*item).to_string()).collect(),
            inventory: BTreeMap::from([(
                cpu(),
                Inventory {
                    total: 8_000,
                    reserved: 1_000,
                    min_unit: 100,
                    max_unit: 7_000,
                    step_size: 100,
                },
            )]),
            disabled: false,
        }
    }

    fn request(amount: u64) -> AllocationRequest {
        AllocationRequest {
            resources: BTreeMap::from([(cpu(), amount)]),
            ..AllocationRequest::default()
        }
    }

    #[test]
    fn validates_provider_hierarchy_and_rejects_cycles() {
        let mut rack = host("rack-a", "az-a", "rack-a", &[]);
        rack.location.host = None;
        rack.inventory.clear();
        let mut child = host("host-a", "az-a", "rack-a", &[]);
        child.parent_id = Some("rack-a".into());

        let topology = TopologySnapshot {
            generation: 7,
            providers: vec![rack, child],
        };
        assert_eq!(topology.validate(), Ok(()));
        assert_eq!(topology.children_of("rack-a").len(), 1);

        let mut cyclic = topology.clone();
        cyclic.providers[0].parent_id = Some("host-a".into());
        assert_eq!(
            cyclic.validate(),
            Err(TopologyError::ParentCycle("rack-a".into()))
        );
    }

    #[test]
    fn candidate_generation_keeps_hard_constraints_separate_from_scoring() {
        let fast = host("fast", "az-a", "rack-a", &["RUNTIME_NULANG", "NVME"]);
        let slow = host("slow", "az-b", "rack-b", &["RUNTIME_NULANG"]);
        let topology = TopologySnapshot {
            generation: 1,
            providers: vec![fast, slow],
        };
        let usage = BTreeMap::from([(
            "fast".into(),
            ProviderUsage {
                allocated: BTreeMap::from([(cpu(), 5_000)]),
            },
        )]);
        let mut req = request(2_000);
        req.required_traits.insert("NVME".into());

        let candidates = allocation_candidates(&topology, &usage, &req).unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].provider.id, "fast");
        assert_eq!(candidates[0].free[&cpu()], 2_000);
    }

    #[test]
    fn forbidden_traits_and_allocation_steps_are_hard_constraints() {
        let spot = host("spot", "az-a", "rack-a", &["SPOT_CAPACITY"]);
        let stable = host("stable", "az-b", "rack-b", &[]);
        let topology = TopologySnapshot {
            generation: 1,
            providers: vec![spot, stable],
        };

        let mut req = request(1_000);
        req.forbidden_traits.insert("SPOT_CAPACITY".into());
        let candidates = allocation_candidates(&topology, &BTreeMap::new(), &req).unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].provider.id, "stable");

        assert!(
            allocation_candidates(&topology, &BTreeMap::new(), &request(250))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn replica_selection_is_deterministic_and_zone_spread() {
        let topology = TopologySnapshot {
            generation: 1,
            providers: vec![
                host("a1", "az-a", "rack-a", &[]),
                host("a2", "az-a", "rack-b", &[]),
                host("b1", "az-b", "rack-c", &[]),
                host("c1", "az-c", "rack-d", &[]),
            ],
        };
        let candidates = allocation_candidates(&topology, &BTreeMap::new(), &request(100)).unwrap();

        let first =
            select_spread_replicas("actor:orders:42", &candidates, 3, FailureDomain::Zone).unwrap();
        let second =
            select_spread_replicas("actor:orders:42", &candidates, 3, FailureDomain::Zone).unwrap();

        let first_ids: Vec<_> = first.iter().map(|provider| provider.id.as_str()).collect();
        let second_ids: Vec<_> = second.iter().map(|provider| provider.id.as_str()).collect();
        assert_eq!(first_ids, second_ids);

        let zones: BTreeSet<_> = first
            .iter()
            .map(|provider| provider.location.zone.as_deref().unwrap())
            .collect();
        assert_eq!(zones.len(), 3);
    }

    #[test]
    fn failure_domain_identity_is_scoped_by_parent_topology() {
        let mut aws = host("aws-a", "zone-a", "rack-1", &[]);
        aws.location.provider = "aws".into();
        let mut gcp = host("gcp-a", "zone-a", "rack-1", &[]);
        gcp.location.provider = "gcp".into();

        assert_ne!(
            aws.location.domain_key(FailureDomain::Zone),
            gcp.location.domain_key(FailureDomain::Zone)
        );

        let mut sibling = host("aws-b", "zone-b", "rack-1", &[]);
        sibling.location.provider = "aws".into();
        assert_ne!(
            aws.location.domain_key(FailureDomain::Rack),
            sibling.location.domain_key(FailureDomain::Rack)
        );

        let topology = TopologySnapshot {
            generation: 1,
            providers: vec![aws, gcp],
        };
        let candidates =
            allocation_candidates(&topology, &BTreeMap::new(), &request(100)).unwrap();
        let selected =
            select_spread_replicas("replicas", &candidates, 2, FailureDomain::Zone).unwrap();
        assert_eq!(selected.len(), 2);
    }

    #[test]
    fn replica_selection_fails_closed_on_insufficient_domains() {
        let topology = TopologySnapshot {
            generation: 1,
            providers: vec![
                host("a1", "az-a", "rack-a", &[]),
                host("a2", "az-a", "rack-b", &[]),
                host("b1", "az-b", "rack-c", &[]),
            ],
        };
        let candidates = allocation_candidates(&topology, &BTreeMap::new(), &request(100)).unwrap();

        assert_eq!(
            select_spread_replicas("stream:payments", &candidates, 3, FailureDomain::Zone),
            Err(TopologyError::InsufficientFailureDomains {
                requested: 3,
                available: 2,
                domain: FailureDomain::Zone,
            })
        );
    }
}
