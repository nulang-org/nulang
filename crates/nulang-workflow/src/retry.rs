use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Backoff {
    None,
    Fixed { delay_ms: u64 },
    Exponential {
        initial_delay_ms: u64,
        max_delay_ms: u64,
        multiplier: u32,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub backoff: Backoff,
}

impl RetryPolicy {
    pub const fn none() -> Self {
        Self {
            max_attempts: 1,
            backoff: Backoff::None,
        }
    }

    pub const fn fixed(max_attempts: u32, delay_ms: u64) -> Self {
        Self {
            max_attempts: if max_attempts == 0 { 1 } else { max_attempts },
            backoff: Backoff::Fixed { delay_ms },
        }
    }

    pub const fn exponential(
        max_attempts: u32,
        initial_delay_ms: u64,
        max_delay_ms: u64,
        multiplier: u32,
    ) -> Self {
        Self {
            max_attempts: if max_attempts == 0 { 1 } else { max_attempts },
            backoff: Backoff::Exponential {
                initial_delay_ms,
                max_delay_ms: if max_delay_ms < initial_delay_ms {
                    initial_delay_ms
                } else {
                    max_delay_ms
                },
                multiplier: if multiplier == 0 { 1 } else { multiplier },
            },
        }
    }

    pub fn should_retry(self, attempt: u32, retryable: bool) -> bool {
        retryable && attempt < self.max_attempts
    }

    /// Delay before the attempt after `failed_attempt`.
    pub fn delay_after_failure(self, failed_attempt: u32) -> Option<u64> {
        if failed_attempt == 0 || failed_attempt >= self.max_attempts {
            return None;
        }

        match self.backoff {
            Backoff::None => Some(0),
            Backoff::Fixed { delay_ms } => Some(delay_ms),
            Backoff::Exponential {
                initial_delay_ms,
                max_delay_ms,
                multiplier,
            } => {
                let mut delay = initial_delay_ms;
                for _ in 1..failed_attempt {
                    delay = delay.saturating_mul(multiplier as u64);
                    if delay >= max_delay_ms {
                        return Some(max_delay_ms);
                    }
                }
                Some(delay.min(max_delay_ms))
            }
        }
    }
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self::none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_exponential_backoff_is_bounded() {
        let policy = RetryPolicy::exponential(6, 100, 1_000, 3);
        assert_eq!(policy.delay_after_failure(1), Some(100));
        assert_eq!(policy.delay_after_failure(2), Some(300));
        assert_eq!(policy.delay_after_failure(3), Some(900));
        assert_eq!(policy.delay_after_failure(4), Some(1_000));
        assert_eq!(policy.delay_after_failure(6), None);
    }

    #[test]
    fn test_zero_attempt_configuration_normalizes_to_one() {
        assert_eq!(RetryPolicy::fixed(0, 50).max_attempts, 1);
        assert_eq!(RetryPolicy::exponential(0, 50, 10, 0).max_attempts, 1);
    }
}
