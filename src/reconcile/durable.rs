use super::{ReconcileAttempt, ReconcileError, ReconcilePhase, ReconcileState};

/// Current durable wire-format version for reconciliation controller snapshots.
pub const RECONCILE_SNAPSHOT_VERSION: u16 = 1;

const RECONCILE_RETRY_TIMER_PREFIX: &str = "__reconcile_retry:";

/// Serializable representation of one reconciliation controller state.
///
/// Restoring this value always goes through [`ReconcileState::restore`], which
/// validates fencing invariants before constructing live state.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReconcileSnapshot<Spec> {
    pub version: u16,
    pub desired: Spec,
    pub generation: u64,
    pub observed_generation: u64,
    pub phase: ReconcilePhase,
    pub attempt_ordinal: u64,
    pub retry_ordinal: u32,
}

/// Fail-closed errors while restoring durable reconciliation state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconcileRestoreError {
    UnsupportedVersion {
        found: u16,
        supported: u16,
    },
    ZeroGeneration,
    ObservedGenerationAhead {
        observed_generation: u64,
        generation: u64,
    },
    TerminalPhaseUnobserved {
        phase: ReconcilePhase,
        observed_generation: u64,
        generation: u64,
    },
    ActivePhaseAlreadyObserved {
        phase: ReconcilePhase,
        observed_generation: u64,
        generation: u64,
    },
    PhaseWithoutAttempt {
        phase: ReconcilePhase,
    },
    RetryOrdinalAheadOfAttempts {
        retry_ordinal: u32,
        attempt_ordinal: u64,
    },
}

/// Retry policy for a reconciliation generation.
///
/// Delay calculation is deterministic and depends only on the retry ordinal
/// and caller-supplied stable jitter seed. No wall-clock reads occur here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReconcileRetryPolicy {
    initial_delay_ms: u64,
    max_delay_ms: u64,
    multiplier: u32,
    max_retries: u32,
    jitter_percent: u8,
}

/// Invalid retry policy configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconcileRetryPolicyError {
    ZeroInitialDelay,
    MaxDelayBelowInitial,
    ZeroMultiplier,
    JitterPercentOutOfRange,
}

impl ReconcileRetryPolicy {
    pub fn new(
        initial_delay_ms: u64,
        max_delay_ms: u64,
        multiplier: u32,
        max_retries: u32,
        jitter_percent: u8,
    ) -> Result<Self, ReconcileRetryPolicyError> {
        if initial_delay_ms == 0 {
            return Err(ReconcileRetryPolicyError::ZeroInitialDelay);
        }
        if max_delay_ms < initial_delay_ms {
            return Err(ReconcileRetryPolicyError::MaxDelayBelowInitial);
        }
        if multiplier == 0 {
            return Err(ReconcileRetryPolicyError::ZeroMultiplier);
        }
        if jitter_percent > 100 {
            return Err(ReconcileRetryPolicyError::JitterPercentOutOfRange);
        }

        Ok(Self {
            initial_delay_ms,
            max_delay_ms,
            multiplier,
            max_retries,
            jitter_percent,
        })
    }

    /// Return the durable timer delay for a 1-based retry ordinal.
    ///
    /// `None` means the retry budget is exhausted (or ordinal zero was passed).
    pub fn delay_ms(&self, retry_ordinal: u32, jitter_seed: u64) -> Option<u64> {
        if retry_ordinal == 0 || retry_ordinal > self.max_retries {
            return None;
        }

        let mut delay = self.initial_delay_ms;
        if self.multiplier > 1 && delay < self.max_delay_ms {
            let mut remaining = retry_ordinal - 1;
            while remaining > 0 && delay < self.max_delay_ms {
                delay = delay
                    .saturating_mul(u64::from(self.multiplier))
                    .min(self.max_delay_ms);
                remaining -= 1;
            }
        }

        if self.jitter_percent == 0 {
            return Some(delay);
        }

        let span = delay
            .saturating_mul(u64::from(self.jitter_percent))
            .saturating_div(100);
        let low = delay.saturating_sub(span);
        let high = delay.saturating_add(span).min(self.max_delay_ms);
        let width = u128::from(high - low) + 1;
        let sample = u128::from(splitmix64(
            jitter_seed ^ u64::from(retry_ordinal).wrapping_mul(0x9E37_79B9_7F4A_7C15),
        ));

        Some((u128::from(low) + sample % width) as u64)
    }
}

/// Fenced identity encoded in a durable reconciliation retry timer name.
///
/// The timer context deliberately carries only identity, not delay: by the time
/// a timer fires, delay has already served its scheduling purpose. Zero values
/// are rejected so malformed or unfenced contexts can never alias live work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct ReconcileRetryIdentity {
    generation: u64,
    retry_ordinal: u32,
}

impl ReconcileRetryIdentity {
    pub fn generation(self) -> u64 {
        self.generation
    }

    pub fn retry_ordinal(self) -> u32 {
        self.retry_ordinal
    }

    pub fn timer_name(self) -> String {
        format!(
            "{RECONCILE_RETRY_TIMER_PREFIX}g{}:r{}",
            self.generation, self.retry_ordinal
        )
    }

    /// Parse a timer context emitted by [`ReconcileRetryTicket::timer_name`].
    ///
    /// Parsing is intentionally strict: extra segments, missing numeric
    /// components, zero generation, and zero retry ordinal are all rejected.
    pub fn parse_timer_name(name: &str) -> Option<Self> {
        let rest = name.strip_prefix(RECONCILE_RETRY_TIMER_PREFIX)?;
        let (generation, retry) = rest.split_once(":r")?;
        if retry.contains(':') || !generation.starts_with('g') {
            return None;
        }
        let generation = generation.strip_prefix('g')?.parse::<u64>().ok()?;
        let retry_ordinal = retry.parse::<u32>().ok()?;
        if generation == 0 || retry_ordinal == 0 {
            return None;
        }
        Some(Self {
            generation,
            retry_ordinal,
        })
    }
}

/// One deterministic retry request that can be mapped onto Nulang's existing
/// durable workflow timer API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReconcileRetryTicket {
    generation: u64,
    retry_ordinal: u32,
    delay_ms: u64,
}

impl ReconcileRetryTicket {
    pub fn generation(self) -> u64 {
        self.generation
    }

    pub fn retry_ordinal(self) -> u32 {
        self.retry_ordinal
    }

    pub fn delay_ms(self) -> u64 {
        self.delay_ms
    }

    pub fn identity(self) -> ReconcileRetryIdentity {
        ReconcileRetryIdentity {
            generation: self.generation,
            retry_ordinal: self.retry_ordinal,
        }
    }

    /// Stable timer name suitable for `Runtime::schedule_workflow_timer`.
    pub fn timer_name(self) -> String {
        self.identity().timer_name()
    }
}

/// Result of applying retry policy to one retryable reconciliation failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconcileRetryDecision {
    Scheduled(ReconcileRetryTicket),
    Exhausted,
}

impl<Spec: Clone> ReconcileState<Spec> {
    /// Capture a durable, versioned snapshot of the controller state.
    pub fn snapshot(&self) -> ReconcileSnapshot<Spec> {
        ReconcileSnapshot {
            version: RECONCILE_SNAPSHOT_VERSION,
            desired: self.desired.clone(),
            generation: self.generation,
            observed_generation: self.observed_generation,
            phase: self.phase,
            attempt_ordinal: self.attempt_ordinal,
            retry_ordinal: self.retry_ordinal,
        }
    }
}

impl<Spec> ReconcileState<Spec> {
    /// Restore a durable snapshot after validating all controller invariants.
    pub fn restore(snapshot: ReconcileSnapshot<Spec>) -> Result<Self, ReconcileRestoreError> {
        if snapshot.version != RECONCILE_SNAPSHOT_VERSION {
            return Err(ReconcileRestoreError::UnsupportedVersion {
                found: snapshot.version,
                supported: RECONCILE_SNAPSHOT_VERSION,
            });
        }
        if snapshot.generation == 0 {
            return Err(ReconcileRestoreError::ZeroGeneration);
        }
        if snapshot.observed_generation > snapshot.generation {
            return Err(ReconcileRestoreError::ObservedGenerationAhead {
                observed_generation: snapshot.observed_generation,
                generation: snapshot.generation,
            });
        }

        let terminal = matches!(
            snapshot.phase,
            ReconcilePhase::Converged | ReconcilePhase::TerminalFailure
        );
        if terminal && snapshot.observed_generation != snapshot.generation {
            return Err(ReconcileRestoreError::TerminalPhaseUnobserved {
                phase: snapshot.phase,
                observed_generation: snapshot.observed_generation,
                generation: snapshot.generation,
            });
        }
        if !terminal && snapshot.observed_generation == snapshot.generation {
            return Err(ReconcileRestoreError::ActivePhaseAlreadyObserved {
                phase: snapshot.phase,
                observed_generation: snapshot.observed_generation,
                generation: snapshot.generation,
            });
        }
        if snapshot.phase != ReconcilePhase::Pending && snapshot.attempt_ordinal == 0 {
            return Err(ReconcileRestoreError::PhaseWithoutAttempt {
                phase: snapshot.phase,
            });
        }
        if u64::from(snapshot.retry_ordinal) > snapshot.attempt_ordinal {
            return Err(ReconcileRestoreError::RetryOrdinalAheadOfAttempts {
                retry_ordinal: snapshot.retry_ordinal,
                attempt_ordinal: snapshot.attempt_ordinal,
            });
        }

        Ok(Self {
            desired: snapshot.desired,
            generation: snapshot.generation,
            observed_generation: snapshot.observed_generation,
            phase: snapshot.phase,
            attempt_ordinal: snapshot.attempt_ordinal,
            retry_ordinal: snapshot.retry_ordinal,
        })
    }

    /// Record a retryable failure and derive its durable timer request.
    ///
    /// When the retry budget is exhausted, the current desired generation is
    /// terminally observed instead of being left in a hot retry loop.
    pub fn mark_retryable_with_policy(
        &mut self,
        attempt: ReconcileAttempt,
        policy: &ReconcileRetryPolicy,
        jitter_seed: u64,
    ) -> Result<ReconcileRetryDecision, ReconcileError> {
        self.require_current(attempt)?;
        let retry_ordinal = self
            .retry_ordinal
            .checked_add(1)
            .ok_or(ReconcileError::RetryOverflow)?;

        let Some(delay_ms) = policy.delay_ms(retry_ordinal, jitter_seed) else {
            self.observed_generation = self.generation;
            self.phase = ReconcilePhase::TerminalFailure;
            return Ok(ReconcileRetryDecision::Exhausted);
        };

        self.retry_ordinal = retry_ordinal;
        self.phase = ReconcilePhase::RetryableFailure;
        Ok(ReconcileRetryDecision::Scheduled(ReconcileRetryTicket {
            generation: self.generation,
            retry_ordinal,
            delay_ms,
        }))
    }

    /// Whether a fired durable retry timer identity still belongs to live
    /// desired state.
    pub fn retry_identity_is_current(&self, identity: ReconcileRetryIdentity) -> bool {
        self.phase == ReconcilePhase::RetryableFailure
            && identity.generation == self.generation
            && identity.retry_ordinal == self.retry_ordinal
    }

    /// Whether a retry ticket still belongs to live desired state.
    pub fn retry_ticket_is_current(&self, ticket: ReconcileRetryTicket) -> bool {
        self.retry_identity_is_current(ticket.identity())
    }
}

fn splitmix64(mut value: u64) -> u64 {
    value = value.wrapping_add(0x9E37_79B9_7F4A_7C15);
    value = (value ^ (value >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    value ^ (value >> 31)
}
