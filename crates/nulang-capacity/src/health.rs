use crate::provider::ProviderError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CircuitBreakerPolicy {
    pub failure_threshold: u32,
    pub base_cooldown_ms: u64,
    pub max_cooldown_ms: u64,
}

impl Default for CircuitBreakerPolicy {
    fn default() -> Self {
        Self {
            failure_threshold: 3,
            base_cooldown_ms: 5_000,
            max_cooldown_ms: 300_000,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ProviderHealth {
    pub consecutive_failures: u32,
    pub open_until_unix_ms: Option<u64>,
}

impl ProviderHealth {
    pub fn is_available(&self, now_unix_ms: u64) -> bool {
        self.open_until_unix_ms
            .map(|until| now_unix_ms >= until)
            .unwrap_or(true)
    }

    pub fn record_success(&mut self) {
        self.consecutive_failures = 0;
        self.open_until_unix_ms = None;
    }

    pub fn record_failure(
        &mut self,
        error: &ProviderError,
        now_unix_ms: u64,
        policy: CircuitBreakerPolicy,
    ) {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);

        let threshold = policy.failure_threshold.max(1);
        if error.retryable && self.consecutive_failures < threshold {
            return;
        }

        let cooldown = if error.retryable {
            let exponent = self.consecutive_failures.saturating_sub(threshold).min(20);
            let multiplier = 1_u64.checked_shl(exponent).unwrap_or(u64::MAX);
            policy
                .base_cooldown_ms
                .saturating_mul(multiplier)
                .min(policy.max_cooldown_ms)
        } else {
            policy.max_cooldown_ms
        };

        self.open_until_unix_ms = Some(now_unix_ms.saturating_add(cooldown));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{ProviderError, ProviderErrorKind};

    fn retryable_error() -> ProviderError {
        ProviderError {
            provider: "aws".into(),
            kind: ProviderErrorKind::Unavailable,
            message: "capacity endpoint unavailable".into(),
            retryable: true,
        }
    }

    #[test]
    fn opens_after_threshold_and_recovers_after_cooldown() {
        let policy = CircuitBreakerPolicy {
            failure_threshold: 2,
            base_cooldown_ms: 1_000,
            max_cooldown_ms: 10_000,
        };
        let mut health = ProviderHealth::default();
        let error = retryable_error();

        health.record_failure(&error, 1_000, policy);
        assert!(health.is_available(1_000));

        health.record_failure(&error, 2_000, policy);
        assert!(!health.is_available(2_999));
        assert!(health.is_available(3_000));
    }

    #[test]
    fn success_resets_breaker() {
        let policy = CircuitBreakerPolicy {
            failure_threshold: 1,
            base_cooldown_ms: 1_000,
            max_cooldown_ms: 10_000,
        };
        let mut health = ProviderHealth::default();
        health.record_failure(&retryable_error(), 1_000, policy);
        assert!(!health.is_available(1_500));

        health.record_success();
        assert_eq!(health.consecutive_failures, 0);
        assert!(health.is_available(1_500));
    }

    #[test]
    fn non_retryable_failure_opens_for_max_cooldown() {
        let policy = CircuitBreakerPolicy {
            failure_threshold: 3,
            base_cooldown_ms: 1_000,
            max_cooldown_ms: 60_000,
        };
        let error = ProviderError {
            provider: "gcp".into(),
            kind: ProviderErrorKind::Authentication,
            message: "invalid credentials".into(),
            retryable: false,
        };
        let mut health = ProviderHealth::default();
        health.record_failure(&error, 10_000, policy);

        assert!(!health.is_available(69_999));
        assert!(health.is_available(70_000));
    }
}
