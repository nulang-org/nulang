use crate::WorkloadClass;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InterruptionReason {
    Rebalance,
    Preemption,
    CapacityReclaim,
    HostMaintenance,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InterruptionNotice {
    pub worker_id: String,
    pub provider: String,
    pub detected_at_unix_ms: u64,
    pub terminate_at_unix_ms: Option<u64>,
    pub reason: InterruptionReason,
    /// Confidence that termination will occur, expressed 0.0..=1.0.
    pub confidence: f64,
}

impl InterruptionNotice {
    pub fn remaining_seconds(&self, now_unix_ms: u64) -> Option<f64> {
        self.terminate_at_unix_ms
            .map(|deadline| deadline.saturating_sub(now_unix_ms) as f64 / 1000.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DrainState {
    Running,
    Draining,
    Checkpointing,
    Released,
    Rescheduled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrainAction {
    StopAcceptingWork,
    Checkpoint,
    ReleaseLease,
    Reschedule,
    Resume,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DrainStep {
    pub state: DrainState,
    pub action: DrainAction,
}

pub fn should_checkpoint(workload_class: WorkloadClass, checkpoint_supported: bool) -> bool {
    checkpoint_supported && workload_class == WorkloadClass::Checkpointable
}

/// Advance one deterministic step through the interruption lifecycle.
///
/// Providers only supply `InterruptionNotice`; the worker executes this common
/// lifecycle regardless of whether the notice originated from AWS rebalance,
/// GCP preemption, Nebius SIGTERM, or another provider-specific mechanism.
pub fn advance_drain(state: DrainState, checkpoint_required: bool) -> DrainStep {
    match state {
        DrainState::Running => DrainStep {
            state: DrainState::Draining,
            action: DrainAction::StopAcceptingWork,
        },
        DrainState::Draining if checkpoint_required => DrainStep {
            state: DrainState::Checkpointing,
            action: DrainAction::Checkpoint,
        },
        DrainState::Draining => DrainStep {
            state: DrainState::Released,
            action: DrainAction::ReleaseLease,
        },
        DrainState::Checkpointing => DrainStep {
            state: DrainState::Released,
            action: DrainAction::ReleaseLease,
        },
        DrainState::Released => DrainStep {
            state: DrainState::Rescheduled,
            action: DrainAction::Reschedule,
        },
        DrainState::Rescheduled => DrainStep {
            state: DrainState::Running,
            action: DrainAction::Resume,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deadline_uses_saturating_time() {
        let notice = InterruptionNotice {
            worker_id: "w1".into(),
            provider: "test".into(),
            detected_at_unix_ms: 1_000,
            terminate_at_unix_ms: Some(61_000),
            reason: InterruptionReason::Preemption,
            confidence: 1.0,
        };
        assert_eq!(notice.remaining_seconds(1_000), Some(60.0));
        assert_eq!(notice.remaining_seconds(62_000), Some(0.0));
    }

    #[test]
    fn checkpointable_work_follows_checkpoint_path() {
        let checkpoint = should_checkpoint(WorkloadClass::Checkpointable, true);
        let draining = advance_drain(DrainState::Running, checkpoint);
        assert_eq!(draining.state, DrainState::Draining);
        assert_eq!(draining.action, DrainAction::StopAcceptingWork);

        let checkpointing = advance_drain(draining.state, checkpoint);
        assert_eq!(checkpointing.state, DrainState::Checkpointing);
        assert_eq!(checkpointing.action, DrainAction::Checkpoint);

        let released = advance_drain(checkpointing.state, checkpoint);
        assert_eq!(released.state, DrainState::Released);
        assert_eq!(released.action, DrainAction::ReleaseLease);
    }

    #[test]
    fn ephemeral_work_skips_checkpoint() {
        let checkpoint = should_checkpoint(WorkloadClass::Ephemeral, true);
        assert!(!checkpoint);
        let released = advance_drain(DrainState::Draining, checkpoint);
        assert_eq!(released.state, DrainState::Released);
    }
}
