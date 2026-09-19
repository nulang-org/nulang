//! Topology-aware placement groups for co-scheduled actors and workers.
//!
//! Group planning is deterministic and provider neutral. It plans against the
//! current durable allocation ledger plus fresh provider heartbeats, while
//! keeping provisional reservations in memory so bundles inside one plan cannot
//! overcommit a resource provider. Persisting the resulting allocations remains
//! a control-plane CAS operation.

use crate::state::{AllocationError, AllocationLedgerSnapshot, CapacityHeartbeatBook};
use crate::topology::{
    allocation_candidates, deterministic_provider_order, AllocationCandidate, AllocationRequest,
    FailureDomain, ProviderUsage, ResourceProvider, TopologyError, TopologySnapshot,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    rename_all = "snake_case",
    tag = "strategy",
    content = "failure_domain"
)]
pub enum PlacementGroupStrategy {
    Pack,
    Spread(FailureDomain),
    StrictPack,
    StrictSpread(FailureDomain),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlacementBundle {
    pub bundle_id: String,
    pub request: AllocationRequest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlacementGroupRequest {
    pub group_id: String,
    /// Stable key used for deterministic rendezvous ordering.
    pub placement_key: String,
    pub strategy: PlacementGroupStrategy,
    pub bundles: Vec<PlacementBundle>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlacementAssignment {
    pub bundle_id: String,
    pub provider_id: String,
    pub failure_domain: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlacementGroupPlan {
    pub group_id: String,
    pub strategy: PlacementGroupStrategy,
    pub assignments: Vec<PlacementAssignment>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum PlacementGroupError {
    #[error("placement group id must not be empty")]
    EmptyGroupId,
    #[error("placement key must not be empty")]
    EmptyPlacementKey,
    #[error("placement group must contain at least one bundle")]
    EmptyBundles,
    #[error("placement bundle id must not be empty")]
    EmptyBundleId,
    #[error("placement bundle id {0} appears more than once")]
    DuplicateBundleId(String),
    #[error("no live capacity candidate can satisfy bundle {bundle_id}")]
    NoCandidate { bundle_id: String },
    #[error("STRICT_PACK cannot fit all bundles on one live resource provider")]
    StrictPackUnsatisfied,
    #[error(
        "bundle {bundle_id} has no candidate exposing requested {domain:?} failure-domain metadata"
    )]
    MissingFailureDomain {
        bundle_id: String,
        domain: FailureDomain,
    },
    #[error(
        "STRICT_SPREAD cannot place bundle {bundle_id} in a new {domain:?} failure domain; {occupied} domains are already occupied"
    )]
    StrictSpreadUnsatisfied {
        bundle_id: String,
        domain: FailureDomain,
        occupied: usize,
    },
    #[error("provisional usage overflow for provider {provider}")]
    UsageOverflow { provider: String },
    #[error(transparent)]
    Topology(#[from] TopologyError),
    #[error(transparent)]
    Allocation(#[from] AllocationError),
}

pub fn plan_placement_group(
    topology: &TopologySnapshot,
    ledger: &AllocationLedgerSnapshot,
    heartbeats: &CapacityHeartbeatBook,
    request: &PlacementGroupRequest,
    now_unix_ms: u64,
    max_heartbeat_age_ms: u64,
) -> Result<PlacementGroupPlan, PlacementGroupError> {
    validate_group(request)?;

    let usage = ledger.usage()?;
    let mut bundles: Vec<&PlacementBundle> = request.bundles.iter().collect();
    bundles.sort_by(|left, right| left.bundle_id.cmp(&right.bundle_id));

    let assignments = match request.strategy {
        PlacementGroupStrategy::StrictPack => plan_strict_pack(
            topology,
            heartbeats,
            &usage,
            &bundles,
            &request.placement_key,
            now_unix_ms,
            max_heartbeat_age_ms,
        )?,
        PlacementGroupStrategy::Pack => plan_pack(
            topology,
            heartbeats,
            usage,
            bundles,
            &request.placement_key,
            now_unix_ms,
            max_heartbeat_age_ms,
        )?,
        PlacementGroupStrategy::Spread(domain) => plan_spread(
            topology,
            heartbeats,
            usage,
            bundles,
            &request.placement_key,
            domain,
            now_unix_ms,
            max_heartbeat_age_ms,
        )?,
        PlacementGroupStrategy::StrictSpread(domain) => plan_strict_spread(
            topology,
            heartbeats,
            &usage,
            &bundles,
            &request.placement_key,
            domain,
            now_unix_ms,
            max_heartbeat_age_ms,
        )?,
    };

    Ok(PlacementGroupPlan {
        group_id: request.group_id.clone(),
        strategy: request.strategy,
        assignments,
    })
}

fn validate_group(request: &PlacementGroupRequest) -> Result<(), PlacementGroupError> {
    if request.group_id.trim().is_empty() {
        return Err(PlacementGroupError::EmptyGroupId);
    }
    if request.placement_key.trim().is_empty() {
        return Err(PlacementGroupError::EmptyPlacementKey);
    }
    if request.bundles.is_empty() {
        return Err(PlacementGroupError::EmptyBundles);
    }

    let mut ids = BTreeSet::new();
    for bundle in &request.bundles {
        if bundle.bundle_id.trim().is_empty() {
            return Err(PlacementGroupError::EmptyBundleId);
        }
        if !ids.insert(bundle.bundle_id.as_str()) {
            return Err(PlacementGroupError::DuplicateBundleId(
                bundle.bundle_id.clone(),
            ));
        }
    }
    Ok(())
}

fn live_candidates<'a>(
    topology: &'a TopologySnapshot,
    heartbeats: &CapacityHeartbeatBook,
    usage: &BTreeMap<String, ProviderUsage>,
    request: &AllocationRequest,
    now_unix_ms: u64,
    max_heartbeat_age_ms: u64,
) -> Result<Vec<AllocationCandidate<'a>>, PlacementGroupError> {
    let mut candidates = allocation_candidates(topology, usage, request)?;
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

fn reserve(
    usage: &mut BTreeMap<String, ProviderUsage>,
    provider: &ResourceProvider,
    request: &AllocationRequest,
) -> Result<(), PlacementGroupError> {
    let provider_usage = usage.entry(provider.id.clone()).or_default();
    for (resource, amount) in &request.resources {
        let current = provider_usage.allocated.get(resource).copied().unwrap_or(0);
        let next =
            current
                .checked_add(*amount)
                .ok_or_else(|| PlacementGroupError::UsageOverflow {
                    provider: provider.id.clone(),
                })?;
        provider_usage.allocated.insert(resource.clone(), next);
    }
    Ok(())
}

fn plan_strict_pack(
    topology: &TopologySnapshot,
    heartbeats: &CapacityHeartbeatBook,
    initial_usage: &BTreeMap<String, ProviderUsage>,
    bundles: &[&PlacementBundle],
    placement_key: &str,
    now_unix_ms: u64,
    max_heartbeat_age_ms: u64,
) -> Result<Vec<PlacementAssignment>, PlacementGroupError> {
    let first = bundles[0];
    let first_candidates = live_candidates(
        topology,
        heartbeats,
        initial_usage,
        &first.request,
        now_unix_ms,
        max_heartbeat_age_ms,
    )?;
    if first_candidates.is_empty() {
        return Err(PlacementGroupError::NoCandidate {
            bundle_id: first.bundle_id.clone(),
        });
    }

    for provider in deterministic_provider_order(placement_key, &first_candidates) {
        let mut usage = initial_usage.clone();
        let mut assignments = Vec::with_capacity(bundles.len());
        let mut fits = true;

        for bundle in bundles {
            let candidates = live_candidates(
                topology,
                heartbeats,
                &usage,
                &bundle.request,
                now_unix_ms,
                max_heartbeat_age_ms,
            )?;
            if !candidates
                .iter()
                .any(|candidate| candidate.provider.id == provider.id)
            {
                fits = false;
                break;
            }

            reserve(&mut usage, provider, &bundle.request)?;
            assignments.push(PlacementAssignment {
                bundle_id: bundle.bundle_id.clone(),
                provider_id: provider.id.clone(),
                failure_domain: None,
            });
        }

        if fits {
            return Ok(assignments);
        }
    }

    Err(PlacementGroupError::StrictPackUnsatisfied)
}

fn choose_most_constrained<'a>(
    topology: &'a TopologySnapshot,
    heartbeats: &CapacityHeartbeatBook,
    usage: &BTreeMap<String, ProviderUsage>,
    remaining: &[&'a PlacementBundle],
    now_unix_ms: u64,
    max_heartbeat_age_ms: u64,
) -> Result<(&'a PlacementBundle, Vec<AllocationCandidate<'a>>), PlacementGroupError> {
    let mut best: Option<(&'a PlacementBundle, Vec<AllocationCandidate<'a>>)> = None;

    for &bundle in remaining {
        let candidates = live_candidates(
            topology,
            heartbeats,
            usage,
            &bundle.request,
            now_unix_ms,
            max_heartbeat_age_ms,
        )?;

        let replace = match &best {
            None => true,
            Some((current_bundle, current_candidates)) => {
                candidates.len() < current_candidates.len()
                    || (candidates.len() == current_candidates.len()
                        && bundle.bundle_id < current_bundle.bundle_id)
            }
        };
        if replace {
            best = Some((bundle, candidates));
        }
    }

    let (bundle, candidates) = best.expect("validated non-empty remaining bundle set");
    if candidates.is_empty() {
        return Err(PlacementGroupError::NoCandidate {
            bundle_id: bundle.bundle_id.clone(),
        });
    }
    Ok((bundle, candidates))
}

fn plan_pack(
    topology: &TopologySnapshot,
    heartbeats: &CapacityHeartbeatBook,
    mut usage: BTreeMap<String, ProviderUsage>,
    mut remaining: Vec<&PlacementBundle>,
    placement_key: &str,
    now_unix_ms: u64,
    max_heartbeat_age_ms: u64,
) -> Result<Vec<PlacementAssignment>, PlacementGroupError> {
    let mut assignments = Vec::with_capacity(remaining.len());
    let mut provider_counts: BTreeMap<String, usize> = BTreeMap::new();

    while !remaining.is_empty() {
        let (bundle, candidates) = choose_most_constrained(
            topology,
            heartbeats,
            &usage,
            &remaining,
            now_unix_ms,
            max_heartbeat_age_ms,
        )?;

        let key = format!("{placement_key}:{}", bundle.bundle_id);
        let order = deterministic_provider_order(&key, &candidates);
        let rank: HashMap<&str, usize> = order
            .iter()
            .enumerate()
            .map(|(index, provider)| (provider.id.as_str(), index))
            .collect();

        let provider = candidates
            .iter()
            .map(|candidate| candidate.provider)
            .min_by_key(|provider| {
                (
                    std::cmp::Reverse(provider_counts.get(&provider.id).copied().unwrap_or(0)),
                    rank.get(provider.id.as_str())
                        .copied()
                        .unwrap_or(usize::MAX),
                )
            })
            .expect("non-empty candidates");

        reserve(&mut usage, provider, &bundle.request)?;
        *provider_counts.entry(provider.id.clone()).or_default() += 1;
        assignments.push(PlacementAssignment {
            bundle_id: bundle.bundle_id.clone(),
            provider_id: provider.id.clone(),
            failure_domain: None,
        });
        remaining.retain(|candidate| candidate.bundle_id != bundle.bundle_id);
    }

    assignments.sort_by(|left, right| left.bundle_id.cmp(&right.bundle_id));
    Ok(assignments)
}

struct StrictSpreadOptions<'a> {
    bundle: &'a PlacementBundle,
    options: Vec<(String, &'a ResourceProvider)>,
}

#[allow(clippy::too_many_arguments)]
fn plan_strict_spread<'a>(
    topology: &'a TopologySnapshot,
    heartbeats: &CapacityHeartbeatBook,
    usage: &BTreeMap<String, ProviderUsage>,
    bundles: &[&'a PlacementBundle],
    placement_key: &str,
    domain: FailureDomain,
    now_unix_ms: u64,
    max_heartbeat_age_ms: u64,
) -> Result<Vec<PlacementAssignment>, PlacementGroupError> {
    let mut all_options = Vec::with_capacity(bundles.len());

    for bundle in bundles {
        let mut candidates = live_candidates(
            topology,
            heartbeats,
            usage,
            &bundle.request,
            now_unix_ms,
            max_heartbeat_age_ms,
        )?;
        candidates.retain(|candidate| candidate.provider.location.domain_value(domain).is_some());

        if candidates.is_empty() {
            return Err(PlacementGroupError::MissingFailureDomain {
                bundle_id: bundle.bundle_id.clone(),
                domain,
            });
        }

        let key = format!("{placement_key}:{}", bundle.bundle_id);
        let ordered = deterministic_provider_order(&key, &candidates);
        let mut seen_domains = BTreeSet::new();
        let mut options = Vec::new();

        for provider in ordered {
            let value = provider
                .location
                .domain_value(domain)
                .expect("filtered candidate has failure domain")
                .to_string();
            if seen_domains.insert(value.clone()) {
                options.push((value, provider));
            }
        }

        all_options.push(StrictSpreadOptions { bundle, options });
    }

    all_options.sort_by(|left, right| {
        left.options
            .len()
            .cmp(&right.options.len())
            .then_with(|| left.bundle.bundle_id.cmp(&right.bundle.bundle_id))
    });

    fn augment<'a>(
        bundle_index: usize,
        all_options: &[StrictSpreadOptions<'a>],
        domain_to_bundle: &mut BTreeMap<String, usize>,
        provider_for_bundle: &mut BTreeMap<usize, &'a ResourceProvider>,
        seen_domains: &mut BTreeSet<String>,
    ) -> bool {
        for (domain_value, provider) in &all_options[bundle_index].options {
            if !seen_domains.insert(domain_value.clone()) {
                continue;
            }

            let occupied_by = domain_to_bundle.get(domain_value).copied();
            let available = match occupied_by {
                None => true,
                Some(other_bundle) => augment(
                    other_bundle,
                    all_options,
                    domain_to_bundle,
                    provider_for_bundle,
                    seen_domains,
                ),
            };

            if available {
                domain_to_bundle.insert(domain_value.clone(), bundle_index);
                provider_for_bundle.insert(bundle_index, *provider);
                return true;
            }
        }
        false
    }

    let mut domain_to_bundle = BTreeMap::new();
    let mut provider_for_bundle = BTreeMap::new();

    for bundle_index in 0..all_options.len() {
        let mut seen_domains = BTreeSet::new();
        if !augment(
            bundle_index,
            &all_options,
            &mut domain_to_bundle,
            &mut provider_for_bundle,
            &mut seen_domains,
        ) {
            return Err(PlacementGroupError::StrictSpreadUnsatisfied {
                bundle_id: all_options[bundle_index].bundle.bundle_id.clone(),
                domain,
                occupied: domain_to_bundle.len(),
            });
        }
    }

    let mut assignments = Vec::with_capacity(all_options.len());
    for (bundle_index, entry) in all_options.iter().enumerate() {
        let provider = provider_for_bundle
            .get(&bundle_index)
            .copied()
            .expect("successful matching assigns provider");
        let domain_value = domain_to_bundle
            .iter()
            .find_map(|(value, assigned_bundle)| {
                (*assigned_bundle == bundle_index).then_some(value.clone())
            })
            .expect("successful matching assigns domain");

        assignments.push(PlacementAssignment {
            bundle_id: entry.bundle.bundle_id.clone(),
            provider_id: provider.id.clone(),
            failure_domain: Some(domain_value),
        });
    }

    assignments.sort_by(|left, right| left.bundle_id.cmp(&right.bundle_id));
    Ok(assignments)
}

#[allow(clippy::too_many_arguments)]
fn plan_spread(
    topology: &TopologySnapshot,
    heartbeats: &CapacityHeartbeatBook,
    mut usage: BTreeMap<String, ProviderUsage>,
    mut remaining: Vec<&PlacementBundle>,
    placement_key: &str,
    domain: FailureDomain,
    now_unix_ms: u64,
    max_heartbeat_age_ms: u64,
) -> Result<Vec<PlacementAssignment>, PlacementGroupError> {
    let mut assignments = Vec::with_capacity(remaining.len());
    let mut domain_counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut provider_counts: BTreeMap<String, usize> = BTreeMap::new();

    while !remaining.is_empty() {
        let (bundle, mut candidates) = choose_most_constrained(
            topology,
            heartbeats,
            &usage,
            &remaining,
            now_unix_ms,
            max_heartbeat_age_ms,
        )?;

        candidates.retain(|candidate| candidate.provider.location.domain_value(domain).is_some());
        if candidates.is_empty() {
            return Err(PlacementGroupError::MissingFailureDomain {
                bundle_id: bundle.bundle_id.clone(),
                domain,
            });
        }

        let key = format!("{placement_key}:{}", bundle.bundle_id);
        let order = deterministic_provider_order(&key, &candidates);
        let rank: HashMap<&str, usize> = order
            .iter()
            .enumerate()
            .map(|(index, provider)| (provider.id.as_str(), index))
            .collect();

        let provider = candidates
            .iter()
            .map(|candidate| candidate.provider)
            .min_by_key(|provider| {
                let value = provider
                    .location
                    .domain_value(domain)
                    .expect("filtered candidate has failure domain");
                (
                    domain_counts.get(value).copied().unwrap_or(0),
                    provider_counts.get(&provider.id).copied().unwrap_or(0),
                    rank.get(provider.id.as_str())
                        .copied()
                        .unwrap_or(usize::MAX),
                )
            });

        let provider = provider.expect("non-empty candidates after domain filtering");

        let domain_value = provider
            .location
            .domain_value(domain)
            .expect("filtered candidate has failure domain")
            .to_string();

        reserve(&mut usage, provider, &bundle.request)?;
        *domain_counts.entry(domain_value.clone()).or_default() += 1;
        *provider_counts.entry(provider.id.clone()).or_default() += 1;
        assignments.push(PlacementAssignment {
            bundle_id: bundle.bundle_id.clone(),
            provider_id: provider.id.clone(),
            failure_domain: Some(domain_value),
        });
        remaining.retain(|candidate| candidate.bundle_id != bundle.bundle_id);
    }

    assignments.sort_by(|left, right| left.bundle_id.cmp(&right.bundle_id));
    Ok(assignments)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{CapacityHeartbeat, CapacityProviderStatus};
    use crate::topology::{
        Inventory, ProviderLocation, ResourceClass, ResourceProvider, RESOURCE_VCPU_MILLIS,
    };

    fn cpu() -> ResourceClass {
        ResourceClass::from(RESOURCE_VCPU_MILLIS)
    }

    fn host(id: &str, zone: &str, total: u64) -> ResourceProvider {
        ResourceProvider {
            id: id.into(),
            parent_id: None,
            location: ProviderLocation {
                provider: "test-cloud".into(),
                region: "us-east".into(),
                zone: Some(zone.into()),
                rack: Some(format!("rack-{id}")),
                host: Some(id.into()),
            },
            traits: BTreeSet::from(["RUNTIME_NULANG".into()]),
            inventory: BTreeMap::from([(
                cpu(),
                Inventory {
                    total,
                    reserved: 0,
                    min_unit: 100,
                    max_unit: total,
                    step_size: 100,
                },
            )]),
            disabled: false,
        }
    }

    fn topology() -> TopologySnapshot {
        TopologySnapshot {
            generation: 4,
            providers: vec![
                host("a", "az-a", 4_000),
                host("b", "az-b", 4_000),
                host("c", "az-c", 4_000),
            ],
        }
    }

    fn heartbeats(topology: &TopologySnapshot) -> CapacityHeartbeatBook {
        let mut book = CapacityHeartbeatBook::default();
        for (sequence, provider) in topology.providers.iter().enumerate() {
            book.apply(
                topology,
                CapacityHeartbeat {
                    provider_id: provider.id.clone(),
                    topology_generation: topology.generation,
                    sequence: sequence as u64 + 1,
                    observed_at_unix_ms: 1_000,
                    status: CapacityProviderStatus::Ready,
                    observed_in_use: BTreeMap::new(),
                },
            )
            .unwrap();
        }
        book
    }

    fn bundle(id: &str, amount: u64) -> PlacementBundle {
        PlacementBundle {
            bundle_id: id.into(),
            request: AllocationRequest {
                resources: BTreeMap::from([(cpu(), amount)]),
                required_traits: BTreeSet::from(["RUNTIME_NULANG".into()]),
                ..AllocationRequest::default()
            },
        }
    }

    fn request(
        strategy: PlacementGroupStrategy,
        bundles: Vec<PlacementBundle>,
    ) -> PlacementGroupRequest {
        PlacementGroupRequest {
            group_id: "group-1".into(),
            placement_key: "deployment:vision-team".into(),
            strategy,
            bundles,
        }
    }

    #[test]
    fn strict_pack_places_every_bundle_on_one_provider() {
        let topology = topology();
        let plan = plan_placement_group(
            &topology,
            &AllocationLedgerSnapshot::default(),
            &heartbeats(&topology),
            &request(
                PlacementGroupStrategy::StrictPack,
                vec![bundle("manager", 1_000), bundle("worker", 2_000)],
            ),
            1_000,
            100,
        )
        .unwrap();

        let providers: BTreeSet<_> = plan
            .assignments
            .iter()
            .map(|assignment| assignment.provider_id.as_str())
            .collect();
        assert_eq!(providers.len(), 1);
    }

    #[test]
    fn strict_pack_fails_when_combined_resources_do_not_fit() {
        let topology = topology();
        assert_eq!(
            plan_placement_group(
                &topology,
                &AllocationLedgerSnapshot::default(),
                &heartbeats(&topology),
                &request(
                    PlacementGroupStrategy::StrictPack,
                    vec![bundle("a", 3_000), bundle("b", 2_000)],
                ),
                1_000,
                100,
            ),
            Err(PlacementGroupError::StrictPackUnsatisfied)
        );
    }

    #[test]
    fn strict_spread_uses_distinct_zones() {
        let topology = topology();
        let plan = plan_placement_group(
            &topology,
            &AllocationLedgerSnapshot::default(),
            &heartbeats(&topology),
            &request(
                PlacementGroupStrategy::StrictSpread(FailureDomain::Zone),
                vec![bundle("a", 500), bundle("b", 500), bundle("c", 500)],
            ),
            1_000,
            100,
        )
        .unwrap();

        let domains: BTreeSet<_> = plan
            .assignments
            .iter()
            .map(|assignment| assignment.failure_domain.as_deref().unwrap())
            .collect();
        assert_eq!(domains.len(), 3);
    }

    #[test]
    fn strict_spread_fails_when_domains_are_exhausted() {
        let mut topology = topology();
        topology.providers[2].location.zone = Some("az-b".into());

        assert!(matches!(
            plan_placement_group(
                &topology,
                &AllocationLedgerSnapshot::default(),
                &heartbeats(&topology),
                &request(
                    PlacementGroupStrategy::StrictSpread(FailureDomain::Zone),
                    vec![bundle("a", 500), bundle("b", 500), bundle("c", 500)],
                ),
                1_000,
                100,
            ),
            Err(PlacementGroupError::StrictSpreadUnsatisfied { .. })
        ));
    }

    #[test]
    fn spread_reuses_domains_only_after_balancing_them() {
        let mut topology = topology();
        topology.providers[2].location.zone = Some("az-b".into());

        let plan = plan_placement_group(
            &topology,
            &AllocationLedgerSnapshot::default(),
            &heartbeats(&topology),
            &request(
                PlacementGroupStrategy::Spread(FailureDomain::Zone),
                vec![
                    bundle("a", 500),
                    bundle("b", 500),
                    bundle("c", 500),
                    bundle("d", 500),
                ],
            ),
            1_000,
            100,
        )
        .unwrap();

        let mut counts = BTreeMap::new();
        for assignment in plan.assignments {
            *counts
                .entry(assignment.failure_domain.unwrap())
                .or_insert(0usize) += 1;
        }
        assert_eq!(counts.len(), 2);
        assert_eq!(counts.values().copied().sum::<usize>(), 4);
        assert!(counts.values().all(|count| *count == 2));
    }

    #[test]
    fn pack_reuses_a_provider_until_capacity_forces_spill() {
        let topology = topology();
        let plan = plan_placement_group(
            &topology,
            &AllocationLedgerSnapshot::default(),
            &heartbeats(&topology),
            &request(
                PlacementGroupStrategy::Pack,
                vec![bundle("a", 2_000), bundle("b", 2_000), bundle("c", 2_000)],
            ),
            1_000,
            100,
        )
        .unwrap();

        let mut counts = BTreeMap::new();
        for assignment in plan.assignments {
            *counts.entry(assignment.provider_id).or_insert(0usize) += 1;
        }
        assert_eq!(counts.values().copied().max(), Some(2));
        assert_eq!(counts.values().copied().sum::<usize>(), 3);
    }

    #[test]
    fn stale_or_draining_provider_is_not_used() {
        let topology = topology();
        let mut book = heartbeats(&topology);
        book.apply(
            &topology,
            CapacityHeartbeat {
                provider_id: "a".into(),
                topology_generation: topology.generation,
                sequence: 99,
                observed_at_unix_ms: 1_000,
                status: CapacityProviderStatus::Draining,
                observed_in_use: BTreeMap::new(),
            },
        )
        .unwrap();

        let plan = plan_placement_group(
            &topology,
            &AllocationLedgerSnapshot::default(),
            &book,
            &request(PlacementGroupStrategy::Pack, vec![bundle("only", 500)]),
            1_000,
            100,
        )
        .unwrap();

        assert_ne!(plan.assignments[0].provider_id, "a");
    }

    #[test]
    fn same_inputs_produce_same_plan() {
        let topology = topology();
        let book = heartbeats(&topology);
        let group = request(
            PlacementGroupStrategy::Spread(FailureDomain::Zone),
            vec![bundle("a", 500), bundle("b", 500), bundle("c", 500)],
        );

        let first = plan_placement_group(
            &topology,
            &AllocationLedgerSnapshot::default(),
            &book,
            &group,
            1_000,
            100,
        )
        .unwrap();
        let second = plan_placement_group(
            &topology,
            &AllocationLedgerSnapshot::default(),
            &book,
            &group,
            1_000,
            100,
        )
        .unwrap();

        assert_eq!(first, second);
    }
}
