//! Actor-specific placement policy mapped onto the provider-neutral capacity model.
//!
//! The language/runtime should describe *what an actor needs*; cloud adapters
//! decide *where capacity comes from*. This module is the translation boundary
//! between those two concerns. It intentionally contains no provider SDK code
//! and no runtime actor implementation details.

use crate::{Architecture, JobSpec, TrustTier, WorkloadClass};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Recovery/availability class for an actor placement.
///
/// This maps directly onto the existing capacity broker's workload classes:
/// transient actors are cheap to recreate, durable actors can recover from a
/// checkpoint/journal, and critical actors prioritize continuity over spot
/// economics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActorContinuity {
    /// Stateless or disposable work that can be restarted from scratch.
    Ephemeral,
    /// Durable/event-sourced work that can resume from persisted progress.
    Durable,
    /// Control-plane or latency-sensitive work where interruption is normally
    /// unacceptable.
    Critical,
}

impl ActorContinuity {
    pub fn workload_class(self) -> WorkloadClass {
        match self {
            ActorContinuity::Ephemeral => WorkloadClass::Ephemeral,
            ActorContinuity::Durable => WorkloadClass::Checkpointable,
            ActorContinuity::Critical => WorkloadClass::Critical,
        }
    }

    /// Conservative default. Callers can explicitly opt durable/ephemeral
    /// actors into interruptible capacity; critical actors remain non-
    /// interruptible unless the caller deliberately overrides the policy.
    pub fn default_allow_interruptible(self) -> bool {
        !matches!(self, ActorContinuity::Critical)
    }
}

/// Minimum resources required by one actor activation or co-scheduled worker.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActorResources {
    pub min_vcpus: f64,
    pub min_memory_gib: f64,
    pub accelerator_model: Option<String>,
    pub min_accelerator_count: u16,
    pub min_accelerator_vram_gib_each: f64,
}

impl Default for ActorResources {
    fn default() -> Self {
        Self {
            min_vcpus: 0.1,
            min_memory_gib: 0.125,
            accelerator_model: None,
            min_accelerator_count: 0,
            min_accelerator_vram_gib_each: 0.0,
        }
    }
}

impl ActorResources {
    pub fn cpu(min_vcpus: f64, min_memory_gib: f64) -> Self {
        Self {
            min_vcpus,
            min_memory_gib,
            ..Self::default()
        }
    }

    pub fn accelerator(
        mut self,
        model: impl Into<String>,
        count: u16,
        min_vram_gib_each: f64,
    ) -> Self {
        self.accelerator_model = Some(model.into());
        self.min_accelerator_count = count;
        self.min_accelerator_vram_gib_each = min_vram_gib_each;
        self
    }
}

/// Provider-neutral actor placement policy.
///
/// This structure is suitable for a Cloud control plane, serialized actor
/// metadata, or a future `@resources` / `@placement` language surface. It does
/// not expose provider names, instance types, or hourly prices.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActorPlacementPolicy {
    pub continuity: ActorContinuity,
    pub architecture: Option<Architecture>,
    pub resources: ActorResources,
    /// Empty means any region is eligible.
    pub allowed_regions: Vec<String>,
    pub min_trust_tier: TrustTier,
    /// Explicit policy knob. Constructors choose a conservative default from
    /// `continuity`, but callers may override it deliberately.
    pub allow_interruptible: bool,
    /// Expected active compute time for one placement. This is an economics
    /// input, not a hard lifetime or lease TTL.
    pub expected_active_seconds: f64,
    /// Recovery checkpoint cadence for interruption-cost estimation.
    pub checkpoint_interval_seconds: Option<f64>,
    pub restart_overhead_seconds: f64,
    /// Expected data leaving the selected capacity during this placement.
    pub data_egress_gib: f64,
}

impl ActorPlacementPolicy {
    pub fn new(continuity: ActorContinuity, resources: ActorResources) -> Self {
        Self {
            continuity,
            architecture: None,
            resources,
            allowed_regions: Vec::new(),
            min_trust_tier: TrustTier::CloudProvider,
            allow_interruptible: continuity.default_allow_interruptible(),
            expected_active_seconds: 300.0,
            checkpoint_interval_seconds: match continuity {
                ActorContinuity::Durable => Some(60.0),
                ActorContinuity::Ephemeral | ActorContinuity::Critical => None,
            },
            restart_overhead_seconds: 15.0,
            data_egress_gib: 0.0,
        }
    }

    pub fn ephemeral(resources: ActorResources) -> Self {
        Self::new(ActorContinuity::Ephemeral, resources)
    }

    pub fn durable(resources: ActorResources) -> Self {
        Self::new(ActorContinuity::Durable, resources)
    }

    pub fn critical(resources: ActorResources) -> Self {
        Self::new(ActorContinuity::Critical, resources)
    }

    /// Validate actor-specific invariants and translate into the existing
    /// capacity broker input. The broker remains the only component that
    /// filters/ranks concrete provider offers.
    pub fn to_job_spec(&self) -> Result<JobSpec, ActorPlacementError> {
        self.validate()?;
        Ok(JobSpec {
            workload_class: self.continuity.workload_class(),
            architecture: self.architecture,
            min_vcpus: self.resources.min_vcpus,
            min_memory_gib: self.resources.min_memory_gib,
            accelerator_model: self.resources.accelerator_model.clone(),
            min_accelerator_count: self.resources.min_accelerator_count,
            min_accelerator_vram_gib_each: self.resources.min_accelerator_vram_gib_each,
            allowed_regions: self.allowed_regions.clone(),
            min_trust_tier: self.min_trust_tier,
            allow_interruptible: self.allow_interruptible,
            nominal_runtime_seconds: self.expected_active_seconds,
            checkpoint_interval_seconds: self.checkpoint_interval_seconds,
            restart_overhead_seconds: self.restart_overhead_seconds,
            data_egress_gib: self.data_egress_gib,
        })
    }

    fn validate(&self) -> Result<(), ActorPlacementError> {
        if !self.resources.min_vcpus.is_finite() || self.resources.min_vcpus < 0.0 {
            return Err(ActorPlacementError::InvalidCpu);
        }
        if !self.resources.min_memory_gib.is_finite() || self.resources.min_memory_gib < 0.0 {
            return Err(ActorPlacementError::InvalidMemory);
        }
        if !self.resources.min_accelerator_vram_gib_each.is_finite()
            || self.resources.min_accelerator_vram_gib_each < 0.0
        {
            return Err(ActorPlacementError::InvalidAcceleratorVram);
        }
        if self.resources.accelerator_model.is_some()
            && self.resources.min_accelerator_count == 0
        {
            return Err(ActorPlacementError::AcceleratorCountRequired);
        }
        if self.resources.accelerator_model.is_none()
            && (self.resources.min_accelerator_count > 0
                || self.resources.min_accelerator_vram_gib_each > 0.0)
        {
            return Err(ActorPlacementError::AcceleratorModelRequired);
        }
        if !self.expected_active_seconds.is_finite() || self.expected_active_seconds < 0.0 {
            return Err(ActorPlacementError::InvalidActiveTime);
        }
        if let Some(interval) = self.checkpoint_interval_seconds {
            if !interval.is_finite() || interval <= 0.0 {
                return Err(ActorPlacementError::InvalidCheckpointInterval);
            }
        }
        if !self.restart_overhead_seconds.is_finite() || self.restart_overhead_seconds < 0.0 {
            return Err(ActorPlacementError::InvalidRestartOverhead);
        }
        if !self.data_egress_gib.is_finite() || self.data_egress_gib < 0.0 {
            return Err(ActorPlacementError::InvalidEgress);
        }
        if self.allowed_regions.iter().any(|region| region.trim().is_empty()) {
            return Err(ActorPlacementError::EmptyRegion);
        }
        Ok(())
    }
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum ActorPlacementError {
    #[error("actor min_vcpus must be finite and non-negative")]
    InvalidCpu,
    #[error("actor min_memory_gib must be finite and non-negative")]
    InvalidMemory,
    #[error("actor accelerator VRAM must be finite and non-negative")]
    InvalidAcceleratorVram,
    #[error("accelerator model requires min_accelerator_count > 0")]
    AcceleratorCountRequired,
    #[error("accelerator count/VRAM requires an accelerator model")]
    AcceleratorModelRequired,
    #[error("expected_active_seconds must be finite and non-negative")]
    InvalidActiveTime,
    #[error("checkpoint_interval_seconds must be finite and greater than zero")]
    InvalidCheckpointInterval,
    #[error("restart_overhead_seconds must be finite and non-negative")]
    InvalidRestartOverhead,
    #[error("data_egress_gib must be finite and non-negative")]
    InvalidEgress,
    #[error("allowed regions cannot contain an empty value")]
    EmptyRegion,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{is_eligible, AcceleratorSpec, CapacityOffer, Lifecycle};

    fn gpu_offer(model: &str, vram: f64, region: &str) -> CapacityOffer {
        CapacityOffer {
            provider: "test-cloud".into(),
            region: region.into(),
            zone: None,
            offer_id: format!("{model}-{vram}"),
            lifecycle: Lifecycle::OnDemand,
            architecture: Architecture::X86_64,
            vcpus: 16.0,
            memory_gib: 64.0,
            accelerator: Some(AcceleratorSpec {
                model: model.into(),
                count: 1,
                vram_gib_each: vram,
            }),
            hourly_usd: 1.0,
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

    #[test]
    fn continuity_maps_to_capacity_workload_class() {
        assert_eq!(
            ActorContinuity::Ephemeral.workload_class(),
            WorkloadClass::Ephemeral
        );
        assert_eq!(
            ActorContinuity::Durable.workload_class(),
            WorkloadClass::Checkpointable
        );
        assert_eq!(
            ActorContinuity::Critical.workload_class(),
            WorkloadClass::Critical
        );
    }

    #[test]
    fn critical_actor_defaults_to_non_interruptible() {
        let policy = ActorPlacementPolicy::critical(ActorResources::default());
        let job = policy.to_job_spec().unwrap();
        assert!(!job.allow_interruptible);
        assert_eq!(job.workload_class, WorkloadClass::Critical);
    }

    #[test]
    fn durable_actor_defaults_to_checkpointable_interruptible_work() {
        let policy = ActorPlacementPolicy::durable(ActorResources::cpu(2.0, 4.0));
        let job = policy.to_job_spec().unwrap();
        assert!(job.allow_interruptible);
        assert_eq!(job.checkpoint_interval_seconds, Some(60.0));
        assert_eq!(job.min_vcpus, 2.0);
        assert_eq!(job.min_memory_gib, 4.0);
    }

    #[test]
    fn gpu_actor_policy_flows_into_hard_constraints() {
        let resources = ActorResources::cpu(4.0, 16.0).accelerator("H100", 1, 80.0);
        let mut policy = ActorPlacementPolicy::durable(resources);
        policy.allowed_regions = vec!["us-east".into()];
        let job = policy.to_job_spec().unwrap();

        assert!(is_eligible(&job, &gpu_offer("H100", 80.0, "us-east")));
        assert!(!is_eligible(&job, &gpu_offer("H100", 40.0, "us-east")));
        assert!(!is_eligible(&job, &gpu_offer("A100", 80.0, "us-east")));
        assert!(!is_eligible(&job, &gpu_offer("H100", 80.0, "eu-west")));
    }

    #[test]
    fn accelerator_constraints_are_not_silently_normalized() {
        let resources = ActorResources {
            accelerator_model: Some("H100".into()),
            min_accelerator_count: 0,
            ..ActorResources::default()
        };
        let policy = ActorPlacementPolicy::durable(resources);
        assert_eq!(
            policy.to_job_spec(),
            Err(ActorPlacementError::AcceleratorCountRequired)
        );
    }

    #[test]
    fn serialized_policy_is_provider_neutral() {
        let policy = ActorPlacementPolicy::ephemeral(ActorResources::cpu(1.0, 2.0));
        let json = serde_json::to_string(&policy).unwrap();
        assert!(json.contains("ephemeral"));
        assert!(!json.contains("aws"));
        assert!(!json.contains("instance_type"));
    }
}
