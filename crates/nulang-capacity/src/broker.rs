use crate::provider::{CapacityProvider, CapacityQuery, ProviderError, ProviderSnapshot};
use crate::{rank_offers, CapacityError, CapacityOffer, ScoreWeights};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BrokerPolicy {
    /// Reject capacity snapshots older than this age. `None` disables expiry.
    pub max_snapshot_age_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PlacementCandidate {
    pub offer: CapacityOffer,
    pub effective_cost_usd: f64,
    pub expected_runtime_seconds: f64,
    pub expected_recovery_seconds: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BrokerResult {
    pub candidates: Vec<PlacementCandidate>,
    pub provider_errors: Vec<ProviderError>,
    pub stale_providers: Vec<String>,
}

impl BrokerResult {
    pub fn best(&self) -> Option<&PlacementCandidate> {
        self.candidates.first()
    }
}

/// Provider-agnostic multi-cloud broker.
///
/// A provider failure is isolated: healthy providers still contribute offers.
/// The broker only returns a `CapacityError` when normalized offer scoring is
/// invalid, not because one cloud endpoint is unavailable.
pub struct CapacityBroker<'a> {
    providers: Vec<&'a dyn CapacityProvider>,
    weights: ScoreWeights,
    policy: BrokerPolicy,
}

impl<'a> CapacityBroker<'a> {
    pub fn new(providers: Vec<&'a dyn CapacityProvider>, weights: ScoreWeights) -> Self {
        Self {
            providers,
            weights,
            policy: BrokerPolicy::default(),
        }
    }

    pub fn with_policy(mut self, policy: BrokerPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Rank without applying snapshot freshness. Useful for deterministic
    /// replay/tests where wall-clock time is intentionally absent.
    pub async fn rank(&self, query: &CapacityQuery) -> Result<BrokerResult, CapacityError> {
        let (snapshots, provider_errors) = self.collect_snapshots(query).await;
        let candidates = rank_snapshots(query, &snapshots, self.weights)?;
        Ok(BrokerResult {
            candidates,
            provider_errors,
            stale_providers: Vec::new(),
        })
    }

    /// Production ranking entry point. Capacity snapshots that exceed the
    /// configured maximum age are excluded before economic scoring.
    pub async fn rank_at(
        &self,
        query: &CapacityQuery,
        now_unix_ms: u64,
    ) -> Result<BrokerResult, CapacityError> {
        let (snapshots, provider_errors) = self.collect_snapshots(query).await;
        let mut fresh = Vec::with_capacity(snapshots.len());
        let mut stale_providers = Vec::new();

        for snapshot in snapshots {
            if snapshot_is_fresh(&snapshot, now_unix_ms, self.policy) {
                fresh.push(snapshot);
            } else {
                stale_providers.push(snapshot.provider);
            }
        }

        let candidates = rank_snapshots(query, &fresh, self.weights)?;
        Ok(BrokerResult {
            candidates,
            provider_errors,
            stale_providers,
        })
    }

    async fn collect_snapshots(
        &self,
        query: &CapacityQuery,
    ) -> (Vec<ProviderSnapshot>, Vec<ProviderError>) {
        let mut snapshots = Vec::with_capacity(self.providers.len());
        let mut provider_errors = Vec::new();

        for provider in &self.providers {
            match provider.fetch_offers(query).await {
                Ok(snapshot) => snapshots.push(snapshot),
                Err(error) => provider_errors.push(error),
            }
        }

        (snapshots, provider_errors)
    }
}

pub fn snapshot_is_fresh(
    snapshot: &ProviderSnapshot,
    now_unix_ms: u64,
    policy: BrokerPolicy,
) -> bool {
    match policy.max_snapshot_age_ms {
        None => true,
        Some(max_age) => now_unix_ms.saturating_sub(snapshot.observed_at_unix_ms) <= max_age,
    }
}

pub fn rank_snapshots(
    query: &CapacityQuery,
    snapshots: &[ProviderSnapshot],
    weights: ScoreWeights,
) -> Result<Vec<PlacementCandidate>, CapacityError> {
    let offers: Vec<CapacityOffer> = snapshots
        .iter()
        .flat_map(|snapshot| snapshot.offers.iter().cloned())
        .collect();

    let ranked = rank_offers(&query.job, &offers, weights)?;
    Ok(ranked
        .into_iter()
        .map(|ranked| PlacementCandidate {
            offer: ranked.offer.clone(),
            effective_cost_usd: ranked.effective_cost_usd,
            expected_runtime_seconds: ranked.expected_runtime_seconds,
            expected_recovery_seconds: ranked.expected_recovery_seconds,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Architecture, CapacityOffer, JobSpec, Lifecycle, TrustTier, WorkloadClass};

    fn offer(provider: &str, id: &str, lifecycle: Lifecycle, hourly_usd: f64) -> CapacityOffer {
        CapacityOffer {
            provider: provider.into(),
            region: "us-east".into(),
            zone: None,
            offer_id: id.into(),
            lifecycle,
            architecture: Architecture::X86_64,
            vcpus: 8.0,
            memory_gib: 32.0,
            accelerator: None,
            hourly_usd,
            storage_usd: 0.0,
            egress_usd_per_gib: 0.0,
            startup_p50_seconds: 2.0,
            startup_p95_seconds: 5.0,
            interruption_rate_per_hour: if lifecycle == Lifecycle::OnDemand {
                0.0
            } else {
                0.01
            },
            interruption_notice_seconds: if lifecycle == Lifecycle::OnDemand {
                0
            } else {
                60
            },
            capacity_confidence: 1.0,
            throughput_score: 1.0,
            trust_tier: TrustTier::CloudProvider,
        }
    }

    fn query(allow_interruptible: bool) -> CapacityQuery {
        CapacityQuery {
            job: JobSpec {
                workload_class: if allow_interruptible {
                    WorkloadClass::Ephemeral
                } else {
                    WorkloadClass::Critical
                },
                architecture: Some(Architecture::X86_64),
                min_vcpus: 2.0,
                min_memory_gib: 4.0,
                accelerator_model: None,
                min_accelerator_count: 0,
                min_accelerator_vram_gib_each: 0.0,
                allowed_regions: vec!["us-east".into()],
                min_trust_tier: TrustTier::CloudProvider,
                allow_interruptible,
                nominal_runtime_seconds: 600.0,
                checkpoint_interval_seconds: None,
                restart_overhead_seconds: 5.0,
                data_egress_gib: 0.0,
            },
            max_offers: 16,
        }
    }

    #[test]
    fn ranks_across_provider_snapshots() {
        let snapshots = vec![
            ProviderSnapshot {
                provider: "aws".into(),
                observed_at_unix_ms: 1,
                offers: vec![offer("aws", "aws-spot", Lifecycle::Spot, 0.10)],
            },
            ProviderSnapshot {
                provider: "gcp".into(),
                observed_at_unix_ms: 1,
                offers: vec![offer("gcp", "gcp-spot", Lifecycle::Spot, 0.08)],
            },
            ProviderSnapshot {
                provider: "nebius".into(),
                observed_at_unix_ms: 1,
                offers: vec![offer(
                    "nebius",
                    "nebius-regular",
                    Lifecycle::OnDemand,
                    0.50,
                )],
            },
        ];

        let ranked = rank_snapshots(&query(true), &snapshots, ScoreWeights::default()).unwrap();
        assert_eq!(ranked[0].offer.offer_id, "gcp-spot");
        assert_eq!(ranked[1].offer.offer_id, "aws-spot");
        assert_eq!(ranked[2].offer.offer_id, "nebius-regular");
    }

    #[test]
    fn on_demand_becomes_fallback_when_interruptible_is_disallowed() {
        let snapshots = vec![
            ProviderSnapshot {
                provider: "aws".into(),
                observed_at_unix_ms: 1,
                offers: vec![offer("aws", "aws-spot", Lifecycle::Spot, 0.01)],
            },
            ProviderSnapshot {
                provider: "nebius".into(),
                observed_at_unix_ms: 1,
                offers: vec![offer("nebius", "regular", Lifecycle::OnDemand, 0.50)],
            },
        ];

        let ranked = rank_snapshots(&query(false), &snapshots, ScoreWeights::default()).unwrap();
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].offer.offer_id, "regular");
    }

    #[test]
    fn stale_snapshot_is_rejected() {
        let snapshot = ProviderSnapshot {
            provider: "aws".into(),
            observed_at_unix_ms: 1_000,
            offers: vec![],
        };
        let policy = BrokerPolicy {
            max_snapshot_age_ms: Some(5_000),
        };

        assert!(snapshot_is_fresh(&snapshot, 6_000, policy));
        assert!(!snapshot_is_fresh(&snapshot, 6_001, policy));
    }
}
