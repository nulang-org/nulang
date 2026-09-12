use crate::interruption::{InterruptionNotice, InterruptionReason};
use crate::provider::{CapacityProvider, CapacityQuery, ProviderError, ProviderFuture, ProviderSnapshot};
use crate::{AcceleratorSpec, Architecture, CapacityOffer, Lifecycle, TrustTier};
use serde::{Deserialize, Serialize};
use std::{future::Future, pin::Pin};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AwsMarket {
    Spot,
    OnDemand,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AwsOffer {
    pub offer_id: String,
    pub region: String,
    pub availability_zone: Option<String>,
    pub instance_type: String,
    pub market: AwsMarket,
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

impl AwsOffer {
    pub fn normalize(self) -> CapacityOffer {
        let lifecycle = match self.market {
            AwsMarket::Spot => Lifecycle::Spot,
            AwsMarket::OnDemand => Lifecycle::OnDemand,
        };
        let accelerator = self.gpu_model.map(|model| AcceleratorSpec {
            model,
            count: self.gpu_count,
            vram_gib_each: self.gpu_vram_gib_each,
        });

        CapacityOffer {
            provider: "aws".into(),
            region: self.region,
            zone: self.availability_zone,
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
            interruption_notice_seconds: if lifecycle == Lifecycle::Spot { 120 } else { 0 },
            capacity_confidence: self.capacity_confidence,
            throughput_score: self.throughput_score,
            trust_tier: TrustTier::CloudProvider,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AwsSnapshot {
    pub observed_at_unix_ms: u64,
    pub offers: Vec<AwsOffer>,
}

pub type AwsSourceFuture<'a> = Pin<Box<dyn Future<Output = Result<AwsSnapshot, ProviderError>> + Send + 'a>>;

pub trait AwsOfferSource: Send + Sync {
    fn fetch<'a>(&'a self, query: &'a CapacityQuery) -> AwsSourceFuture<'a>;
}

pub struct AwsCapacityAdapter<S> {
    source: S,
}

impl<S> AwsCapacityAdapter<S> {
    pub fn new(source: S) -> Self {
        Self { source }
    }
}

impl<S: AwsOfferSource> CapacityProvider for AwsCapacityAdapter<S> {
    fn provider_id(&self) -> &str {
        "aws"
    }

    fn fetch_offers<'a>(&'a self, query: &'a CapacityQuery) -> ProviderFuture<'a> {
        Box::pin(async move {
            let raw = self.source.fetch(query).await?;
            let offers = raw
                .offers
                .into_iter()
                .take(query.max_offers)
                .map(AwsOffer::normalize)
                .collect();
            Ok(ProviderSnapshot {
                provider: "aws".into(),
                observed_at_unix_ms: raw.observed_at_unix_ms,
                offers,
            })
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AwsInterruptionEvent {
    pub worker_id: String,
    pub detected_at_unix_ms: u64,
    pub terminate_at_unix_ms: Option<u64>,
    pub rebalance_recommendation: bool,
}

impl AwsInterruptionEvent {
    pub fn normalize(self) -> InterruptionNotice {
        InterruptionNotice {
            worker_id: self.worker_id,
            provider: "aws".into(),
            detected_at_unix_ms: self.detected_at_unix_ms,
            terminate_at_unix_ms: self.terminate_at_unix_ms,
            reason: if self.rebalance_recommendation {
                InterruptionReason::Rebalance
            } else {
                InterruptionReason::CapacityReclaim
            },
            confidence: if self.terminate_at_unix_ms.is_some() { 1.0 } else { 0.5 },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spot_offer_gets_aws_notice_window() {
        let offer = AwsOffer {
            offer_id: "c7g-spot".into(),
            region: "us-east-1".into(),
            availability_zone: Some("us-east-1a".into()),
            instance_type: "c7g.2xlarge".into(),
            market: AwsMarket::Spot,
            architecture: Architecture::Arm64,
            vcpus: 8.0,
            memory_gib: 16.0,
            gpu_model: None,
            gpu_count: 0,
            gpu_vram_gib_each: 0.0,
            hourly_usd: 0.11,
            storage_usd: 0.0,
            egress_usd_per_gib: 0.09,
            startup_p50_seconds: 20.0,
            startup_p95_seconds: 60.0,
            interruption_rate_per_hour: 0.02,
            capacity_confidence: 0.9,
            throughput_score: 1.0,
        }
        .normalize();

        assert_eq!(offer.provider, "aws");
        assert_eq!(offer.lifecycle, Lifecycle::Spot);
        assert_eq!(offer.interruption_notice_seconds, 120);
        assert_eq!(offer.trust_tier, TrustTier::CloudProvider);
    }
}
