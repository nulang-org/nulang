use crate::{CapacityOffer, JobSpec};
use serde::{Deserialize, Serialize};
use std::{future::Future, pin::Pin};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CapacityQuery {
    pub job: JobSpec,
    /// Upper bound requested from a provider. Providers may return fewer offers.
    pub max_offers: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderSnapshot {
    pub provider: String,
    pub observed_at_unix_ms: u64,
    pub offers: Vec<CapacityOffer>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderErrorKind {
    Authentication,
    RateLimited,
    Unavailable,
    InvalidResponse,
    Configuration,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderError {
    pub provider: String,
    pub kind: ProviderErrorKind,
    pub message: String,
    pub retryable: bool,
}

pub type ProviderFuture<'a> = Pin<
    Box<dyn Future<Output = Result<ProviderSnapshot, ProviderError>> + Send + 'a>,
>;

/// Async boundary implemented by cloud-specific adapter crates.
///
/// The capacity core deliberately knows nothing about AWS, GCP, Nebius, or
/// any other provider SDK. Implementations fetch native inventory/pricing and
/// normalize it into `CapacityOffer` values before returning.
pub trait CapacityProvider: Send + Sync {
    fn provider_id(&self) -> &str;

    fn fetch_offers<'a>(&'a self, query: &'a CapacityQuery) -> ProviderFuture<'a>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Architecture, TrustTier, WorkloadClass};

    #[test]
    fn query_is_provider_neutral() {
        let query = CapacityQuery {
            job: JobSpec {
                workload_class: WorkloadClass::Ephemeral,
                architecture: Some(Architecture::Arm64),
                min_vcpus: 4.0,
                min_memory_gib: 8.0,
                accelerator_model: None,
                min_accelerator_count: 0,
                min_accelerator_vram_gib_each: 0.0,
                allowed_regions: vec!["us-east".into()],
                min_trust_tier: TrustTier::CloudProvider,
                allow_interruptible: true,
                nominal_runtime_seconds: 120.0,
                checkpoint_interval_seconds: None,
                restart_overhead_seconds: 5.0,
                data_egress_gib: 0.0,
            },
            max_offers: 16,
        };

        let encoded = serde_json::to_string(&query).unwrap();
        assert!(encoded.contains("arm64"));
        assert!(!encoded.contains("aws"));
        assert!(!encoded.contains("gcp"));
    }
}
