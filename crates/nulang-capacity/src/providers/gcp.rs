use crate::interruption::{InterruptionNotice, InterruptionReason};
use crate::provider::{
    CapacityProvider, CapacityQuery, ProviderError, ProviderFuture, ProviderSnapshot,
};
use crate::{AcceleratorSpec, Architecture, CapacityOffer, Lifecycle, TrustTier};
use serde::{Deserialize, Serialize};
use std::{future::Future, pin::Pin};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GcpProvisioningModel {
    Spot,
    Standard,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GcpOffer {
    pub offer_id: String,
    pub region: String,
    pub zone: Option<String>,
    pub machine_type: String,
    pub provisioning_model: GcpProvisioningModel,
    pub architecture: Architecture,
    pub vcpus: f64,
    pub memory_gib: f64,
    pub gpu_model: Option<String>,
    pub gpu_count: u16,
    pub gpu_vram_gib_each: f64,
    pub hourly_usd: f64,
    pub storage_usd: f64,
    pub egress_usd_per_gib: f64,
    pub startup_p50_seconds: f64,
    pub startup_p95_seconds: f64,
    pub interruption_rate_per_hour: f64,
    pub interruption_notice_seconds: u32,
    pub capacity_confidence: f64,
    pub throughput_score: f64,
}

impl GcpOffer {
    pub fn normalize(self) -> CapacityOffer {
        let lifecycle = match self.provisioning_model {
            GcpProvisioningModel::Spot => Lifecycle::Spot,
            GcpProvisioningModel::Standard => Lifecycle::OnDemand,
        };
        let accelerator = self.gpu_model.map(|model| AcceleratorSpec {
            model,
            count: self.gpu_count,
            vram_gib_each: self.gpu_vram_gib_each,
        });

        CapacityOffer {
            provider: "gcp".into(),
            region: self.region,
            zone: self.zone,
            offer_id: self.offer_id,
            lifecycle,
            architecture: self.architecture,
            vcpus: self.vcpus,
            memory_gib: self.memory_gib,
            accelerator,
            hourly_usd: self.hourly_usd,
            storage_usd: self.storage_usd,
            egress_usd_per_gib: self.egress_usd_per_gib,
            startup_p50_seconds: self.startup_p50_seconds,
            startup_p95_seconds: self.startup_p95_seconds,
            interruption_rate_per_hour: if lifecycle == Lifecycle::Spot {
                self.interruption_rate_per_hour
            } else {
                0.0
            },
            interruption_notice_seconds: if lifecycle == Lifecycle::Spot {
                self.interruption_notice_seconds
            } else {
                0
            },
            capacity_confidence: self.capacity_confidence,
            throughput_score: self.throughput_score,
            trust_tier: TrustTier::CloudProvider,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GcpSnapshot {
    pub observed_at_unix_ms: u64,
    pub offers: Vec<GcpOffer>,
}

pub type GcpSourceFuture<'a> =
    Pin<Box<dyn Future<Output = Result<GcpSnapshot, ProviderError>> + Send + 'a>>;

pub trait GcpOfferSource: Send + Sync {
    fn fetch<'a>(&'a self, query: &'a CapacityQuery) -> GcpSourceFuture<'a>;
}

pub struct GcpCapacityAdapter<S> {
    source: S,
}

impl<S> GcpCapacityAdapter<S> {
    pub fn new(source: S) -> Self {
        Self { source }
    }
}

impl<S: GcpOfferSource> CapacityProvider for GcpCapacityAdapter<S> {
    fn provider_id(&self) -> &str {
        "gcp"
    }

    fn fetch_offers<'a>(&'a self, query: &'a CapacityQuery) -> ProviderFuture<'a> {
        Box::pin(async move {
            let raw = self.source.fetch(query).await?;
            let offers = raw
                .offers
                .into_iter()
                .take(query.max_offers)
                .map(GcpOffer::normalize)
                .collect();
            Ok(ProviderSnapshot {
                provider: "gcp".into(),
                observed_at_unix_ms: raw.observed_at_unix_ms,
                offers,
            })
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GcpPreemptionEvent {
    pub worker_id: String,
    pub detected_at_unix_ms: u64,
    pub terminate_at_unix_ms: Option<u64>,
}

impl GcpPreemptionEvent {
    pub fn normalize(self) -> InterruptionNotice {
        InterruptionNotice {
            worker_id: self.worker_id,
            provider: "gcp".into(),
            detected_at_unix_ms: self.detected_at_unix_ms,
            terminate_at_unix_ms: self.terminate_at_unix_ms,
            reason: InterruptionReason::Preemption,
            confidence: 1.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configured_spot_notice_window_survives_normalization() {
        let offer = GcpOffer {
            offer_id: "a2-spot".into(),
            region: "us-central1".into(),
            zone: Some("us-central1-a".into()),
            machine_type: "a2-highgpu-1g".into(),
            provisioning_model: GcpProvisioningModel::Spot,
            architecture: Architecture::X86_64,
            vcpus: 12.0,
            memory_gib: 85.0,
            gpu_model: Some("A100".into()),
            gpu_count: 1,
            gpu_vram_gib_each: 40.0,
            hourly_usd: 2.0,
            storage_usd: 0.0,
            egress_usd_per_gib: 0.12,
            startup_p50_seconds: 30.0,
            startup_p95_seconds: 90.0,
            interruption_rate_per_hour: 0.03,
            interruption_notice_seconds: 30,
            capacity_confidence: 0.8,
            throughput_score: 1.0,
        }
        .normalize();

        assert_eq!(offer.provider, "gcp");
        assert_eq!(offer.lifecycle, Lifecycle::Spot);
        assert_eq!(offer.interruption_notice_seconds, 30);
        assert_eq!(offer.accelerator.unwrap().model, "A100");
    }
}
