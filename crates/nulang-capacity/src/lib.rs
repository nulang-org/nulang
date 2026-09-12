use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use thiserror::Error;

pub mod broker;
pub mod interruption;
pub mod provider;
pub mod providers;
pub mod telemetry;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Lifecycle {
    Spot,
    Preemptible,
    OnDemand,
    Reserved,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Architecture {
    X86_64,
    Arm64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustTier {
    Untrusted,
    VerifiedMarketplace,
    CloudProvider,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkloadClass {
    Critical,
    Checkpointable,
    Ephemeral,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AcceleratorSpec {
    pub model: String,
    pub count: u16,
    pub vram_gib_each: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CapacityOffer {
    pub provider: String,
    pub region: String,
    pub zone: Option<String>,
    pub offer_id: String,
    pub lifecycle: Lifecycle,
    pub architecture: Architecture,
    pub vcpus: f64,
    pub memory_gib: f64,
    pub accelerator: Option<AcceleratorSpec>,
    pub hourly_usd: f64,
    pub storage_usd: f64,
    pub egress_usd_per_gib: f64,
    pub startup_p50_seconds: f64,
    pub startup_p95_seconds: f64,
    /// Historical interruption probability per running hour, expressed 0.0..=1.0.
    pub interruption_rate_per_hour: f64,
    pub interruption_notice_seconds: u32,
    /// Historical probability that this offer can actually be acquired now, 0.0..=1.0.
    pub capacity_confidence: f64,
    /// Relative observed throughput where 1.0 is the reference machine.
    pub throughput_score: f64,
    pub trust_tier: TrustTier,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobSpec {
    pub workload_class: WorkloadClass,
    pub architecture: Option<Architecture>,
    pub min_vcpus: f64,
    pub min_memory_gib: f64,
    pub accelerator_model: Option<String>,
    pub min_accelerator_count: u16,
    pub min_accelerator_vram_gib_each: f64,
    pub allowed_regions: Vec<String>,
    pub min_trust_tier: TrustTier,
    /// Explicit workload policy. If false, Spot/preemptible offers are rejected.
    pub allow_interruptible: bool,
    pub nominal_runtime_seconds: f64,
    pub checkpoint_interval_seconds: Option<f64>,
    pub restart_overhead_seconds: f64,
    pub data_egress_gib: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ScoreWeights {
    pub startup_seconds_usd: f64,
    pub acquisition_failure_usd: f64,
    pub interruption_usd: f64,
}

impl Default for ScoreWeights {
    fn default() -> Self {
        Self {
            startup_seconds_usd: 0.00002,
            acquisition_failure_usd: 0.10,
            interruption_usd: 0.05,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RankedOffer<'a> {
    pub offer: &'a CapacityOffer,
    pub effective_cost_usd: f64,
    pub expected_runtime_seconds: f64,
    pub expected_recovery_seconds: f64,
}

#[derive(Debug, Error, PartialEq)]
pub enum CapacityError {
    #[error("job nominal_runtime_seconds must be non-negative")]
    NegativeRuntime,
    #[error("offer {0} has a non-positive throughput_score")]
    NonPositiveThroughput(String),
    #[error("offer {0} contains an invalid probability outside 0.0..=1.0")]
    InvalidProbability(String),
}

pub fn is_eligible(job: &JobSpec, offer: &CapacityOffer) -> bool {
    if let Some(arch) = job.architecture {
        if offer.architecture != arch {
            return false;
        }
    }
    if offer.vcpus < job.min_vcpus || offer.memory_gib < job.min_memory_gib {
        return false;
    }
    if !job.allow_interruptible
        && matches!(offer.lifecycle, Lifecycle::Spot | Lifecycle::Preemptible)
    {
        return false;
    }
    if offer.trust_tier < job.min_trust_tier {
        return false;
    }
    if !job.allowed_regions.is_empty() && !job.allowed_regions.iter().any(|r| r == &offer.region) {
        return false;
    }

    match (&job.accelerator_model, &offer.accelerator) {
        (None, _) if job.min_accelerator_count == 0 => {}
        (requested, Some(acc)) => {
            if acc.count < job.min_accelerator_count
                || acc.vram_gib_each < job.min_accelerator_vram_gib_each
            {
                return false;
            }
            if let Some(model) = requested {
                if !acc.model.eq_ignore_ascii_case(model) {
                    return false;
                }
            }
        }
        _ => return false,
    }

    true
}

pub fn score_offer<'a>(
    job: &JobSpec,
    offer: &'a CapacityOffer,
    weights: ScoreWeights,
) -> Result<RankedOffer<'a>, CapacityError> {
    if job.nominal_runtime_seconds < 0.0 {
        return Err(CapacityError::NegativeRuntime);
    }
    if offer.throughput_score <= 0.0 {
        return Err(CapacityError::NonPositiveThroughput(offer.offer_id.clone()));
    }
    if !(0.0..=1.0).contains(&offer.interruption_rate_per_hour)
        || !(0.0..=1.0).contains(&offer.capacity_confidence)
    {
        return Err(CapacityError::InvalidProbability(offer.offer_id.clone()));
    }

    let runtime = job.nominal_runtime_seconds / offer.throughput_score;
    let runtime_hours = runtime / 3600.0;
    let expected_interruptions = offer.interruption_rate_per_hour * runtime_hours;

    let lost_work_seconds = job
        .checkpoint_interval_seconds
        .map(|interval| interval.max(0.0) / 2.0)
        .unwrap_or(runtime / 2.0);
    let recovery_per_interrupt = lost_work_seconds + job.restart_overhead_seconds.max(0.0);
    let recovery_seconds = expected_interruptions * recovery_per_interrupt;
    let compute_seconds = runtime + recovery_seconds;

    let compute_cost = offer.hourly_usd.max(0.0) * compute_seconds / 3600.0;
    let data_cost = offer.storage_usd.max(0.0)
        + offer.egress_usd_per_gib.max(0.0) * job.data_egress_gib.max(0.0);
    let startup_penalty = offer.startup_p95_seconds.max(0.0) * weights.startup_seconds_usd;
    let acquisition_penalty = (1.0 - offer.capacity_confidence) * weights.acquisition_failure_usd;
    let interruption_penalty = expected_interruptions * weights.interruption_usd;

    Ok(RankedOffer {
        offer,
        effective_cost_usd: compute_cost
            + data_cost
            + startup_penalty
            + acquisition_penalty
            + interruption_penalty,
        expected_runtime_seconds: runtime,
        expected_recovery_seconds: recovery_seconds,
    })
}

pub fn rank_offers<'a>(
    job: &JobSpec,
    offers: &'a [CapacityOffer],
    weights: ScoreWeights,
) -> Result<Vec<RankedOffer<'a>>, CapacityError> {
    let mut ranked = offers
        .iter()
        .filter(|offer| is_eligible(job, offer))
        .map(|offer| score_offer(job, offer, weights))
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

    fn offer(id: &str, lifecycle: Lifecycle, hourly: f64) -> CapacityOffer {
        CapacityOffer {
            provider: "test".into(),
            region: "us-east".into(),
            zone: None,
            offer_id: id.into(),
            lifecycle,
            architecture: Architecture::X86_64,
            vcpus: 8.0,
            memory_gib: 32.0,
            accelerator: None,
            hourly_usd: hourly,
            storage_usd: 0.0,
            egress_usd_per_gib: 0.0,
            startup_p50_seconds: 5.0,
            startup_p95_seconds: 10.0,
            interruption_rate_per_hour: 0.0,
            interruption_notice_seconds: 120,
            capacity_confidence: 1.0,
            throughput_score: 1.0,
            trust_tier: TrustTier::CloudProvider,
        }
    }

    fn job(class: WorkloadClass, runtime: f64) -> JobSpec {
        JobSpec {
            workload_class: class,
            architecture: None,
            min_vcpus: 1.0,
            min_memory_gib: 1.0,
            accelerator_model: None,
            min_accelerator_count: 0,
            min_accelerator_vram_gib_each: 0.0,
            allowed_regions: vec![],
            min_trust_tier: TrustTier::CloudProvider,
            allow_interruptible: class != WorkloadClass::Critical,
            nominal_runtime_seconds: runtime,
            checkpoint_interval_seconds: Some(60.0),
            restart_overhead_seconds: 15.0,
            data_egress_gib: 0.0,
        }
    }

    #[test]
    fn cheap_spot_wins_for_short_ephemeral_work() {
        let spot = offer("spot", Lifecycle::Spot, 0.25);
        let ondemand = offer("ondemand", Lifecycle::OnDemand, 1.00);
        let ranked = rank_offers(
            &job(WorkloadClass::Ephemeral, 20.0),
            &[ondemand, spot],
            ScoreWeights::default(),
        )
        .unwrap();
        assert_eq!(ranked[0].offer.offer_id, "spot");
    }

    #[test]
    fn interruption_heavy_offer_can_lose_despite_lower_hourly_price() {
        let mut flaky = offer("flaky", Lifecycle::Spot, 0.50);
        flaky.interruption_rate_per_hour = 0.95;
        let reliable = offer("reliable", Lifecycle::OnDemand, 0.80);
        let mut workload = job(WorkloadClass::Checkpointable, 8.0 * 3600.0);
        workload.checkpoint_interval_seconds = Some(1800.0);
        workload.restart_overhead_seconds = 300.0;
        let weights = ScoreWeights {
            interruption_usd: 2.0,
            ..ScoreWeights::default()
        };
        let ranked = rank_offers(&workload, &[flaky, reliable], weights).unwrap();
        assert_eq!(ranked[0].offer.offer_id, "reliable");
    }

    #[test]
    fn egress_can_erase_compute_savings() {
        let mut remote = offer("remote", Lifecycle::Spot, 0.10);
        remote.egress_usd_per_gib = 0.10;
        let local = offer("local", Lifecycle::OnDemand, 0.40);
        let mut workload = job(WorkloadClass::Ephemeral, 3600.0);
        workload.data_egress_gib = 10.0;
        let ranked = rank_offers(
            &workload,
            &[remote, local],
            ScoreWeights::default(),
        )
        .unwrap();
        assert_eq!(ranked[0].offer.offer_id, "local");
    }

    #[test]
    fn trust_and_vram_are_hard_constraints() {
        let mut marketplace = offer("market", Lifecycle::Spot, 0.05);
        marketplace.trust_tier = TrustTier::VerifiedMarketplace;
        marketplace.accelerator = Some(AcceleratorSpec {
            model: "H100".into(),
            count: 1,
            vram_gib_each: 80.0,
        });
        let mut cloud = offer("cloud", Lifecycle::OnDemand, 2.00);
        cloud.accelerator = Some(AcceleratorSpec {
            model: "H100".into(),
            count: 1,
            vram_gib_each: 40.0,
        });
        let mut workload = job(WorkloadClass::Checkpointable, 3600.0);
        workload.accelerator_model = Some("H100".into());
        workload.min_accelerator_count = 1;
        workload.min_accelerator_vram_gib_each = 80.0;
        workload.min_trust_tier = TrustTier::CloudProvider;
        let ranked = rank_offers(
            &workload,
            &[marketplace, cloud],
            ScoreWeights::default(),
        )
        .unwrap();
        assert!(ranked.is_empty());
    }

    #[test]
    fn critical_jobs_reject_interruptible_capacity_by_default() {
        let spot = offer("spot", Lifecycle::Spot, 0.01);
        let ondemand = offer("ondemand", Lifecycle::OnDemand, 1.00);
        let ranked = rank_offers(
            &job(WorkloadClass::Critical, 600.0),
            &[spot, ondemand],
            ScoreWeights::default(),
        )
        .unwrap();
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].offer.offer_id, "ondemand");
    }
}
