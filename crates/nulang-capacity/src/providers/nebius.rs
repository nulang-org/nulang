use crate::interruption::{InterruptionNotice, InterruptionReason};
use crate::provider::{
    CapacityProvider, CapacityQuery, ProviderError, ProviderFuture, ProviderSnapshot,
};
use crate::{AcceleratorSpec, Architecture, CapacityOffer, Lifecycle, TrustTier};
use serde::{Deserialize, Serialize};
use std::{future::Future, pin::Pin};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NebiusMarket {
    Preemptible,
    Regular,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NebiusOffer {
    pub offer_id: String,
    pub region: String,
    pub zone: Option<String>,
    pub platform: String,
    pub market: NebiusMarket,
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
    pub capacity_confidence: f64,
    pub throughput_score: f64,
}

impl NebiusOffer {
    pub fn normalize(self) -> CapacityOffer {
        let lifecycle = match self.market {
            NebiusMarket::Preemptible => Lifecycle::Preemptible,
            NebiusMarket::Regular => Lifecycle::OnDemand,
        };
        let accelerator = self.gpu_model.map(|model| AcceleratorSpec {
            model,
            count: self.gpu_count,
            vram_gib_each: self.gpu_vram_gib_each,
        });

        CapacityOffer {
            provider: "nebius".into(),
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
            interruption_rate_per_hour: if lifecycle == Lifecycle::Preemptible {
                self.interruption_rate_per_hour
            } else {
                0.0
            },
            interruption_notice_seconds: if lifecycle == Lifecycle::Preemptible {
                60
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
pub struct NebiusSnapshot {
    pub observed_at_unix_ms: u64,
    pub offers: Vec<NebiusOffer>,
}

pub type NebiusSourceFuture<'a> =
    Pin<Box<dyn Future<Output = Result<NebiusSnapshot, ProviderError>> + Send + 'a>>;

pub trait NebiusOfferSource: Send + Sync {
    fn fetch<'a>(&'a self, query: &'a CapacityQuery) -> NebiusSourceFuture<'a>;
}

pub struct NebiusCapacityAdapter<S> {
    source: S,
}

impl<S> NebiusCapacityAdapter<S> {
    pub fn new(source: S) -> Self {
        Self { source }
    }
}

impl<S: NebiusOfferSource> CapacityProvider for NebiusCapacityAdapter<S> {
    fn provider_id(&self) -> &str {
        "nebius"
    }

    fn fetch_offers<'a>(&'a self, query: &'a CapacityQuery) -> ProviderFuture<'a> {
        Box::pin(async move {
            let raw = self.source.fetch(query).await?;
            let offers = raw
                .offers
                .into_iter()
                .take(query.max_offers)
                .map(NebiusOffer::normalize)
                .collect();
            Ok(ProviderSnapshot {
                provider: "nebius".into(),
                observed_at_unix_ms: raw.observed_at_unix_ms,
                offers,
            })
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NebiusPreemptionEvent {
    pub worker_id: String,
    pub detected_at_unix_ms: u64,
    pub terminate_at_unix_ms: Option<u64>,
}

impl NebiusPreemptionEvent {
    pub fn normalize(self) -> InterruptionNotice {
        InterruptionNotice {
            worker_id: self.worker_id,
            provider: "nebius".into(),
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
    fn preemptible_gpu_offer_gets_nebius_notice_window() {
        let offer = NebiusOffer {
            offer_id: "h100-preemptible".into(),
            region: "eu-north1".into(),
            zone: None,
            platform: "h100".into(),
            market: NebiusMarket::Preemptible,
            architecture: Architecture::X86_64,
            vcpus: 16.0,
            memory_gib: 128.0,
            gpu_model: Some("H100".into()),
            gpu_count: 1,
            gpu_vram_gib_each: 80.0,
            hourly_usd: 2.15,
            storage_usd: 0.0,
            egress_usd_per_gib: 0.0,
            startup_p50_seconds: 20.0,
            startup_p95_seconds: 60.0,
            interruption_rate_per_hour: 0.02,
            capacity_confidence: 0.85,
            throughput_score: 1.0,
        }
        .normalize();

        assert_eq!(offer.provider, "nebius");
        assert_eq!(offer.lifecycle, Lifecycle::Preemptible);
        assert_eq!(offer.interruption_notice_seconds, 60);
        assert_eq!(offer.accelerator.unwrap().model, "H100");
    }
}
