//! Data-locality scoring for actor/capacity placement.
//!
//! `nulang-capacity` currently ranks provider offers from compute economics,
//! interruption risk, startup latency, trust, and hard resource constraints.
//! Large immutable objects add another cost: moving model weights, tensors,
//! embeddings, media, or snapshots to a region where no usable replica exists.
//!
//! This module keeps that concern provider-neutral. Callers describe where an
//! object already has replicas and the observed cost/time to materialize a
//! remote copy. The locality scorer then adds those penalties to the existing
//! `score_offer` result without teaching the capacity core about S3, R2, GCS,
//! NVMe, or a specific object-store implementation.
//!
//! Phase 1 is deliberately provider/region/zone aware rather than node-memory
//! aware. RAM/VRAM/NVMe placement requires concrete host topology, which the
//! current `CapacityOffer` model does not represent yet.

use crate::{is_eligible, score_offer, CapacityError, CapacityOffer, JobSpec, ScoreWeights};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;

const BYTES_PER_GIB: f64 = 1024.0 * 1024.0 * 1024.0;

/// A known usable replica of an immutable object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectReplica {
    pub provider: String,
    pub region: String,
    /// `None` means the replica is region-scoped (for example an object-store
    /// bucket) and can be treated as local to any zone in the region.
    pub zone: Option<String>,
}

impl ObjectReplica {
    pub fn regional(provider: impl Into<String>, region: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            region: region.into(),
            zone: None,
        }
    }

    pub fn zonal(
        provider: impl Into<String>,
        region: impl Into<String>,
        zone: impl Into<String>,
    ) -> Self {
        Self {
            provider: provider.into(),
            region: region.into(),
            zone: Some(zone.into()),
        }
    }

    /// Whether this replica can be consumed without the remote-materialization
    /// penalty for a concrete capacity offer.
    pub fn is_local_to(&self, offer: &CapacityOffer) -> bool {
        if !self.provider.eq_ignore_ascii_case(&offer.provider) || self.region != offer.region {
            return false;
        }

        match (&self.zone, &offer.zone) {
            // Region-scoped replica is usable from every zone in that region.
            (None, _) => true,
            // Zonal replica is local only when the offer identifies that zone.
            (Some(replica_zone), Some(offer_zone)) => replica_zone == offer_zone,
            (Some(_), None) => false,
        }
    }
}

/// One immutable input object needed by an actor/workload placement.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObjectDependency {
    pub object_id: String,
    pub size_bytes: u64,
    /// Existing replicas known to the scheduler/control plane.
    pub replicas: Vec<ObjectReplica>,
    /// Expected incremental cost to materialize one GiB when no local replica
    /// exists. This should come from provider/object-store policy or observed
    /// telemetry rather than being inferred from destination compute price.
    pub remote_transfer_usd_per_gib: f64,
    /// Expected wall-clock seconds per GiB to materialize a remote copy.
    pub remote_transfer_seconds_per_gib: f64,
}

impl ObjectDependency {
    pub fn new(object_id: impl Into<String>, size_bytes: u64) -> Self {
        Self {
            object_id: object_id.into(),
            size_bytes,
            replicas: Vec::new(),
            remote_transfer_usd_per_gib: 0.0,
            remote_transfer_seconds_per_gib: 0.0,
        }
    }

    pub fn with_replica(mut self, replica: ObjectReplica) -> Self {
        self.replicas.push(replica);
        self
    }

    pub fn with_remote_transfer(
        mut self,
        usd_per_gib: f64,
        seconds_per_gib: f64,
    ) -> Self {
        self.remote_transfer_usd_per_gib = usd_per_gib;
        self.remote_transfer_seconds_per_gib = seconds_per_gib;
        self
    }

    pub fn size_gib(&self) -> f64 {
        self.size_bytes as f64 / BYTES_PER_GIB
    }

    fn is_local_to(&self, offer: &CapacityOffer) -> bool {
        self.replicas.iter().any(|replica| replica.is_local_to(offer))
    }
}

/// Immutable-object locality inputs for one placement decision.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LocalityProfile {
    pub objects: Vec<ObjectDependency>,
}

impl LocalityProfile {
    pub fn new(objects: Vec<ObjectDependency>) -> Self {
        Self { objects }
    }

    pub fn is_empty(&self) -> bool {
        self.objects.is_empty()
    }

    /// Aggregate remote-materialization impact for one capacity offer.
    ///
    /// Objects with any provider/region/(optional zone) local replica incur no
    /// penalty. Every other object contributes its full materialization size.
    pub fn impact_for(&self, offer: &CapacityOffer) -> LocalityImpact {
        let mut impact = LocalityImpact::default();

        for object in &self.objects {
            if object.is_local_to(offer) {
                continue;
            }

            let size_gib = object.size_gib();
            impact.transfer_gib += size_gib;
            impact.transfer_cost_usd +=
                size_gib * object.remote_transfer_usd_per_gib.max(0.0);
            impact.transfer_seconds +=
                size_gib * object.remote_transfer_seconds_per_gib.max(0.0);
            impact.remote_object_count += 1;
        }

        impact
    }
}

/// Additional placement cost caused by missing local object replicas.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct LocalityImpact {
    pub transfer_gib: f64,
    pub transfer_cost_usd: f64,
    pub transfer_seconds: f64,
    pub remote_object_count: usize,
}

/// Existing capacity score plus transparent locality accounting.
#[derive(Debug, Clone, PartialEq)]
pub struct LocalityRankedOffer<'a> {
    pub offer: &'a CapacityOffer,
    pub effective_cost_usd: f64,
    pub expected_runtime_seconds: f64,
    pub expected_recovery_seconds: f64,
    pub locality: LocalityImpact,
}

/// Score an eligible offer with object-locality penalties.
///
/// Transfer dollars are added directly. Transfer time is converted through the
/// existing startup-time weight so callers keep one latency-to-dollar policy.
pub fn score_offer_with_locality<'a>(
    job: &JobSpec,
    offer: &'a CapacityOffer,
    profile: &LocalityProfile,
    weights: ScoreWeights,
) -> Result<LocalityRankedOffer<'a>, CapacityError> {
    let base = score_offer(job, offer, weights)?;
    let locality = profile.impact_for(offer);
    let transfer_time_penalty = locality.transfer_seconds * weights.startup_seconds_usd.max(0.0);

    Ok(LocalityRankedOffer {
        offer,
        effective_cost_usd: base.effective_cost_usd
            + locality.transfer_cost_usd
            + transfer_time_penalty,
        expected_runtime_seconds: base.expected_runtime_seconds,
        expected_recovery_seconds: base.expected_recovery_seconds,
        locality,
    })
}

/// Rank eligible offers with object locality included in the effective cost.
pub fn rank_offers_with_locality<'a>(
    job: &JobSpec,
    offers: &'a [CapacityOffer],
    profile: &LocalityProfile,
    weights: ScoreWeights,
) -> Result<Vec<LocalityRankedOffer<'a>>, CapacityError> {
    let mut ranked = offers
        .iter()
        .filter(|offer| is_eligible(job, offer))
        .map(|offer| score_offer_with_locality(job, offer, profile, weights))
        .collect::<Result<Vec<_>, _>>()?;

    ranked.sort_by(|a, b| {
        a.effective_cost_usd
            .partial_cmp(&b.effective_cost_usd)
            .unwrap_or(Ordering::Equal)
            .then_with(|| a.offer.provider.cmp(&b.offer.provider))
            .then_with(|| a.offer.offer_id.cmp(&b.offer.offer_id))
    });

    Ok(ranked)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Architecture, Lifecycle, TrustTier, WorkloadClass};

    fn offer(provider: &str, region: &str, id: &str, hourly_usd: f64) -> CapacityOffer {
        CapacityOffer {
            provider: provider.into(),
            region: region.into(),
            zone: Some(format!("{region}-a")),
            offer_id: id.into(),
            lifecycle: Lifecycle::OnDemand,
            architecture: Architecture::X86_64,
            vcpus: 8.0,
            memory_gib: 32.0,
            accelerator: None,
            hourly_usd,
            storage_usd: 0.0,
            egress_usd_per_gib: 0.0,
            startup_p50_seconds: 0.0,
            startup_p95_seconds: 0.0,
            interruption_rate_per_hour: 0.0,
            interruption_notice_seconds: 120,
            capacity_confidence: 1.0,
            throughput_score: 1.0,
            trust_tier: TrustTier::CloudProvider,
        }
    }

    fn job() -> JobSpec {
        JobSpec {
            workload_class: WorkloadClass::Ephemeral,
            architecture: None,
            min_vcpus: 1.0,
            min_memory_gib: 1.0,
            accelerator_model: None,
            min_accelerator_count: 0,
            min_accelerator_vram_gib_each: 0.0,
            allowed_regions: Vec::new(),
            min_trust_tier: TrustTier::CloudProvider,
            allow_interruptible: true,
            nominal_runtime_seconds: 3600.0,
            checkpoint_interval_seconds: None,
            restart_overhead_seconds: 0.0,
            data_egress_gib: 0.0,
        }
    }

    fn gib(n: u64) -> u64 {
        n * 1024 * 1024 * 1024
    }

    #[test]
    fn region_local_replica_has_zero_materialization_penalty() {
        let east = offer("cloud-a", "us-east", "east", 1.0);
        let profile = LocalityProfile::new(vec![ObjectDependency::new("weights", gib(40))
            .with_replica(ObjectReplica::regional("cloud-a", "us-east"))
            .with_remote_transfer(0.10, 2.0)]);

        assert_eq!(profile.impact_for(&east), LocalityImpact::default());
    }

    #[test]
    fn zonal_replica_requires_matching_offer_zone() {
        let mut east = offer("cloud-a", "us-east", "east", 1.0);
        let profile = LocalityProfile::new(vec![ObjectDependency::new("cache", gib(10))
            .with_replica(ObjectReplica::zonal("cloud-a", "us-east", "us-east-b"))
            .with_remote_transfer(0.02, 1.0)]);

        let remote = profile.impact_for(&east);
        assert_eq!(remote.remote_object_count, 1);
        assert_eq!(remote.transfer_gib, 10.0);

        east.zone = Some("us-east-b".into());
        assert_eq!(profile.impact_for(&east), LocalityImpact::default());
    }

    #[test]
    fn locality_can_outweigh_cheaper_remote_compute() {
        let local = offer("cloud-a", "us-east", "local", 1.00);
        let remote = offer("cloud-b", "us-west", "remote", 0.50);
        let offers = [remote, local];
        let profile = LocalityProfile::new(vec![ObjectDependency::new("model", gib(100))
            .with_replica(ObjectReplica::regional("cloud-a", "us-east"))
            .with_remote_transfer(0.02, 0.0)]);

        let ranked = rank_offers_with_locality(
            &job(),
            &offers,
            &profile,
            ScoreWeights::default(),
        )
        .unwrap();

        assert_eq!(ranked[0].offer.offer_id, "local");
        assert_eq!(ranked[0].locality.remote_object_count, 0);
        assert_eq!(ranked[1].locality.transfer_gib, 100.0);
        assert_eq!(ranked[1].locality.transfer_cost_usd, 2.0);
    }

    #[test]
    fn transfer_time_uses_existing_startup_latency_weight() {
        let local = offer("cloud-a", "us-east", "local", 1.0);
        let remote = offer("cloud-b", "us-west", "remote", 1.0);
        let profile = LocalityProfile::new(vec![ObjectDependency::new("tensor", gib(10))
            .with_replica(ObjectReplica::regional("cloud-a", "us-east"))
            .with_remote_transfer(0.0, 5.0)]);
        let weights = ScoreWeights {
            startup_seconds_usd: 0.01,
            acquisition_failure_usd: 0.0,
            interruption_usd: 0.0,
        };

        let local_score = score_offer_with_locality(&job(), &local, &profile, weights).unwrap();
        let remote_score = score_offer_with_locality(&job(), &remote, &profile, weights).unwrap();

        assert_eq!(remote_score.locality.transfer_seconds, 50.0);
        assert!((remote_score.effective_cost_usd - local_score.effective_cost_usd - 0.5).abs() < 1e-9);
    }

    #[test]
    fn multiple_replicas_make_any_matching_region_local() {
        let west = offer("cloud-b", "us-west", "west", 1.0);
        let profile = LocalityProfile::new(vec![ObjectDependency::new("snapshot", gib(5))
            .with_replica(ObjectReplica::regional("cloud-a", "us-east"))
            .with_replica(ObjectReplica::regional("cloud-b", "us-west"))
            .with_remote_transfer(1.0, 10.0)]);

        assert_eq!(profile.impact_for(&west), LocalityImpact::default());
    }

    #[test]
    fn empty_profile_preserves_base_ranking() {
        let expensive = offer("cloud-a", "us-east", "expensive", 2.0);
        let cheap = offer("cloud-b", "us-west", "cheap", 1.0);
        let offers = [expensive, cheap];

        let ranked = rank_offers_with_locality(
            &job(),
            &offers,
            &LocalityProfile::default(),
            ScoreWeights::default(),
        )
        .unwrap();

        assert_eq!(ranked[0].offer.offer_id, "cheap");
        assert_eq!(ranked[0].locality, LocalityImpact::default());
    }
}
