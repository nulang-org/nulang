use crate::provider::{
    CapacityProvider, CapacityQuery, ProviderError, ProviderErrorKind, ProviderSnapshot,
};
use crate::{rank_offers, CapacityError, CapacityOffer, ScoreWeights};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BrokerPolicy {
    /// Reject capacity snapshots older than this age. `None` disables expiry.
    pub max_snapshot_age_ms: Option<u64>,
    /// Per-provider fetch deadline. A provider that does not answer within
    /// this window is recorded as a retryable `ProviderErrorKind::Unavailable`
    /// and contributes no offers; healthy providers are unaffected.
    /// Defaults to 10s so a hung endpoint can never stall ranking.
    pub fetch_timeout: Option<std::time::Duration>,
}

impl Default for BrokerPolicy {
    fn default() -> Self {
        Self {
            max_snapshot_age_ms: None,
            fetch_timeout: Some(std::time::Duration::from_secs(10)),
        }
    }
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
        use futures::future::{self, Either, FutureExt};
        use futures_timer::Delay;

        // Fan out across providers so N endpoint latencies overlap instead of
        // adding, and race each fetch against the policy deadline so a hung
        // provider is isolated as a retryable error instead of stalling
        // ranking forever.
        let fetches = self
            .providers
            .iter()
            .map(|provider| {
                let fetch = provider.fetch_offers(query);
                async move {
                    match self.policy.fetch_timeout {
                        Some(timeout) => match future::select(fetch, Delay::new(timeout)).await {
                            Either::Left((result, _delay)) => result,
                            Either::Right(((), _fetch)) => Err(ProviderError {
                                provider: provider.provider_id().to_string(),
                                kind: ProviderErrorKind::Unavailable,
                                message: format!("fetch timed out after {timeout:?}"),
                                retryable: true,
                            }),
                        },
                        None => fetch.await,
                    }
                }
                .boxed()
            })
            .collect::<Vec<_>>();

        let results = future::join_all(fetches).await;
        let mut snapshots = Vec::with_capacity(results.len());
        let mut provider_errors = Vec::new();
        for result in results {
            match result {
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
                offers: vec![offer("nebius", "nebius-regular", Lifecycle::OnDemand, 0.50)],
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
            fetch_timeout: None,
        };

        assert!(snapshot_is_fresh(&snapshot, 6_000, policy));
        assert!(!snapshot_is_fresh(&snapshot, 6_001, policy));
    }

    use crate::provider::{CapacityProvider, ProviderFuture};
    use futures_timer::Delay;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    enum MockBehavior {
        Ready,
        Sleep(Duration),
        /// Completes only once the shared flag is set (by another provider's
        /// fetch). Under sequential collection this never runs the peer, so
        /// the fetch would spin forever; under concurrent fan-out it completes.
        WaitsFor(Arc<AtomicBool>),
    }

    struct MockProvider {
        id: &'static str,
        behavior: MockBehavior,
        snapshot: ProviderSnapshot,
    }

    impl CapacityProvider for MockProvider {
        fn provider_id(&self) -> &str {
            self.id
        }

        fn fetch_offers<'a>(&'a self, _query: &'a CapacityQuery) -> ProviderFuture<'a> {
            match &self.behavior {
                MockBehavior::Ready => {
                    let snapshot = self.snapshot.clone();
                    Box::pin(async move { Ok(snapshot) })
                }
                MockBehavior::Sleep(duration) => {
                    let snapshot = self.snapshot.clone();
                    let duration = *duration;
                    Box::pin(async move {
                        Delay::new(duration).await;
                        Ok(snapshot)
                    })
                }
                MockBehavior::WaitsFor(flag) => {
                    let flag = flag.clone();
                    Box::pin(async move {
                        while !flag.load(Ordering::SeqCst) {
                            // Zero-delay timer as a portable yield.
                            Delay::new(Duration::from_millis(0)).await;
                        }
                        Err(ProviderError {
                            provider: "released".into(),
                            kind: crate::provider::ProviderErrorKind::Unavailable,
                            message: "released".into(),
                            retryable: true,
                        })
                    })
                }
            }
        }
    }

    fn snapshot(provider: &str, offer_id: &str) -> ProviderSnapshot {
        ProviderSnapshot {
            provider: provider.into(),
            observed_at_unix_ms: 1,
            offers: vec![offer(provider, offer_id, Lifecycle::OnDemand, 0.50)],
        }
    }

    #[test]
    fn timed_out_provider_is_isolated_as_retryable_error() {
        let fast = MockProvider {
            id: "fast",
            behavior: MockBehavior::Ready,
            snapshot: snapshot("fast", "fast-1"),
        };
        let hung = MockProvider {
            id: "hung",
            behavior: MockBehavior::Sleep(Duration::from_secs(60)),
            snapshot: snapshot("hung", "hung-1"),
        };
        let broker = CapacityBroker::new(vec![&fast, &hung], ScoreWeights::default()).with_policy(
            BrokerPolicy {
                max_snapshot_age_ms: None,
                fetch_timeout: Some(Duration::from_millis(50)),
            },
        );

        let result = futures::executor::block_on(broker.rank(&query(false))).unwrap();
        assert_eq!(result.candidates.len(), 1);
        assert_eq!(result.candidates[0].offer.offer_id, "fast-1");
        assert_eq!(result.provider_errors.len(), 1);
        let error = &result.provider_errors[0];
        assert_eq!(error.provider, "hung");
        assert_eq!(error.kind, crate::provider::ProviderErrorKind::Unavailable);
        assert!(error.retryable);
        assert!(error.message.contains("timed out"));
    }

    #[test]
    fn provider_fetches_run_concurrently() {
        // Provider A only completes after provider B's fetch sets the flag.
        // Sequential collection would never poll B, so A would spin forever;
        // concurrent fan-out completes.
        let flag = Arc::new(AtomicBool::new(false));
        let waiting = MockProvider {
            id: "waiting",
            behavior: MockBehavior::WaitsFor(flag.clone()),
            snapshot: snapshot("waiting", "waiting-1"),
        };
        let releasing = MockProvider {
            id: "releasing",
            behavior: MockBehavior::Ready,
            snapshot: snapshot("releasing", "releasing-1"),
        };
        // Set the flag from the releasing provider via a wrapper: polling
        // order is not guaranteed, so the flag must be set by whichever
        // provider's fetch actually runs.
        struct ReleasingProvider {
            flag: Arc<AtomicBool>,
            inner: MockProvider,
        }
        impl CapacityProvider for ReleasingProvider {
            fn provider_id(&self) -> &str {
                self.inner.id
            }
            fn fetch_offers<'a>(&'a self, query: &'a CapacityQuery) -> ProviderFuture<'a> {
                self.flag.store(true, Ordering::SeqCst);
                self.inner.fetch_offers(query)
            }
        }
        let releasing = ReleasingProvider {
            flag,
            inner: releasing,
        };

        let broker = CapacityBroker::new(vec![&waiting, &releasing], ScoreWeights::default())
            .with_policy(BrokerPolicy {
                max_snapshot_age_ms: None,
                fetch_timeout: Some(Duration::from_secs(5)),
            });

        let result = futures::executor::block_on(broker.rank(&query(false))).unwrap();
        // The waiting provider was released, errored by design, and the
        // releasing provider's offer is ranked.
        assert_eq!(result.candidates.len(), 1);
        assert_eq!(result.candidates[0].offer.offer_id, "releasing-1");
        assert_eq!(result.provider_errors.len(), 1);
        assert_eq!(result.provider_errors[0].provider, "released");
    }
}
