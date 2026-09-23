use crate::CapacityOffer;

#[derive(Debug, Clone, PartialEq, Default)]
pub struct OfferTelemetryWindow {
    pub acquisition_attempts: u64,
    pub acquisition_successes: u64,
    pub runtime_seconds: f64,
    pub interruptions: u64,
    /// Runtime the same completed work would take on the reference machine.
    pub reference_work_seconds: f64,
    pub startup_samples_seconds: Vec<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OfferEstimate {
    pub capacity_confidence: f64,
    pub interruption_rate_per_hour: f64,
    pub throughput_score: f64,
    pub startup_p50_seconds: f64,
    pub startup_p95_seconds: f64,
}

impl OfferTelemetryWindow {
    pub fn estimate(&self) -> Option<OfferEstimate> {
        if self.acquisition_attempts == 0 && self.runtime_seconds <= 0.0 {
            return None;
        }

        let capacity_confidence = if self.acquisition_attempts == 0 {
            1.0
        } else {
            self.acquisition_successes.min(self.acquisition_attempts) as f64
                / self.acquisition_attempts as f64
        };

        let interruption_rate_per_hour = if self.runtime_seconds <= 0.0 {
            0.0
        } else {
            let runtime_hours = self.runtime_seconds / 3600.0;
            let hazard_per_hour = self.interruptions as f64 / runtime_hours;
            // Convert a Poisson event intensity into probability of at least
            // one interruption during one running hour.
            (1.0 - (-hazard_per_hour).exp()).clamp(0.0, 1.0)
        };

        let throughput_score = if self.runtime_seconds > 0.0 && self.reference_work_seconds > 0.0 {
            self.reference_work_seconds / self.runtime_seconds
        } else {
            1.0
        };

        let mut startup = self
            .startup_samples_seconds
            .iter()
            .copied()
            .filter(|value| value.is_finite() && *value >= 0.0)
            .collect::<Vec<_>>();
        startup.sort_by(|a, b| a.total_cmp(b));

        let startup_p50_seconds = percentile(&startup, 0.50).unwrap_or(0.0);
        let startup_p95_seconds = percentile(&startup, 0.95).unwrap_or(startup_p50_seconds);

        Some(OfferEstimate {
            capacity_confidence,
            interruption_rate_per_hour,
            throughput_score,
            startup_p50_seconds,
            startup_p95_seconds,
        })
    }
}

pub fn apply_estimate(offer: &mut CapacityOffer, estimate: OfferEstimate) {
    offer.capacity_confidence = estimate.capacity_confidence.clamp(0.0, 1.0);
    offer.interruption_rate_per_hour = estimate.interruption_rate_per_hour.clamp(0.0, 1.0);
    offer.throughput_score = estimate.throughput_score.max(f64::EPSILON);
    offer.startup_p50_seconds = estimate.startup_p50_seconds.max(0.0);
    offer.startup_p95_seconds = estimate.startup_p95_seconds.max(offer.startup_p50_seconds);
}

fn percentile(sorted: &[f64], quantile: f64) -> Option<f64> {
    if sorted.is_empty() {
        return None;
    }
    let index = ((sorted.len() - 1) as f64 * quantile.clamp(0.0, 1.0)).round() as usize;
    sorted.get(index).copied()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Architecture, Lifecycle, TrustTier};

    fn offer() -> CapacityOffer {
        CapacityOffer {
            provider: "test".into(),
            region: "us-east".into(),
            zone: None,
            offer_id: "offer".into(),
            lifecycle: Lifecycle::Spot,
            architecture: Architecture::X86_64,
            vcpus: 8.0,
            memory_gib: 32.0,
            accelerator: None,
            hourly_usd: 1.0,
            storage_usd: 0.0,
            egress_usd_per_gib: 0.0,
            startup_p50_seconds: 30.0,
            startup_p95_seconds: 60.0,
            interruption_rate_per_hour: 0.1,
            interruption_notice_seconds: 120,
            capacity_confidence: 0.5,
            throughput_score: 1.0,
            trust_tier: TrustTier::CloudProvider,
        }
    }

    #[test]
    fn estimates_observed_fleet_characteristics() {
        let telemetry = OfferTelemetryWindow {
            acquisition_attempts: 10,
            acquisition_successes: 8,
            runtime_seconds: 7200.0,
            interruptions: 1,
            reference_work_seconds: 10_800.0,
            startup_samples_seconds: vec![4.0, 5.0, 6.0, 7.0, 20.0],
        };
        let estimate = telemetry.estimate().unwrap();

        assert!((estimate.capacity_confidence - 0.8).abs() < 1e-9);
        assert!((estimate.throughput_score - 1.5).abs() < 1e-9);
        assert_eq!(estimate.startup_p50_seconds, 6.0);
        assert_eq!(estimate.startup_p95_seconds, 20.0);
        assert!(estimate.interruption_rate_per_hour > 0.0);
        assert!(estimate.interruption_rate_per_hour < 1.0);
    }

    #[test]
    fn estimate_overrides_catalog_assumptions() {
        let mut offer = offer();
        apply_estimate(
            &mut offer,
            OfferEstimate {
                capacity_confidence: 0.95,
                interruption_rate_per_hour: 0.02,
                throughput_score: 1.3,
                startup_p50_seconds: 3.0,
                startup_p95_seconds: 8.0,
            },
        );

        assert_eq!(offer.capacity_confidence, 0.95);
        assert_eq!(offer.interruption_rate_per_hour, 0.02);
        assert_eq!(offer.throughput_score, 1.3);
        assert_eq!(offer.startup_p95_seconds, 8.0);
    }
}
