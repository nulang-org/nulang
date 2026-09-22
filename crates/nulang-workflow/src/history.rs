use crate::RetryPolicy;
use blake3::Hasher;
use serde::{Deserialize, Serialize};
use std::fmt;

const ACTIVITY_INVOCATION_DOMAIN: &[u8] = b"nulang.workflow.activity.v1\0";

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct WorkflowId(String);

impl WorkflowId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for WorkflowId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ActivityInvocationId([u8; 32]);

impl ActivityInvocationId {
    pub fn derive(
        workflow_id: &WorkflowId,
        step: &str,
        occurrence: u32,
        operation: &str,
    ) -> Self {
        let mut hasher = Hasher::new();
        hasher.update(ACTIVITY_INVOCATION_DOMAIN);
        hash_len_prefixed(&mut hasher, workflow_id.as_str().as_bytes());
        hash_len_prefixed(&mut hasher, step.as_bytes());
        hasher.update(&occurrence.to_le_bytes());
        hash_len_prefixed(&mut hasher, operation.as_bytes());
        Self(*hasher.finalize().as_bytes())
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn idempotency_key(&self) -> String {
        self.to_string()
    }
}

impl fmt::Display for ActivityInvocationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in &self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

fn hash_len_prefixed(hasher: &mut Hasher, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkflowEvent {
    ActivityPrepared {
        invocation_id: ActivityInvocationId,
        step: String,
        operation: String,
        request: Vec<u8>,
        retry: RetryPolicy,
    },
    ActivityAttemptFailed {
        invocation_id: ActivityInvocationId,
        attempt: u32,
        error: String,
        retryable: bool,
        next_retry_at_millis: Option<u64>,
    },
    ActivityCompleted {
        invocation_id: ActivityInvocationId,
        result: Vec<u8>,
    },
    SagaStarted {
        saga_id: String,
        plan_hash: [u8; 32],
    },
    SagaStepCommitted {
        saga_id: String,
        step_index: u32,
        step_name: String,
    },
    SagaFailed {
        saga_id: String,
        failed_step_index: u32,
        failed_step_name: String,
        error: String,
    },
    SagaStepCompensated {
        saga_id: String,
        step_index: u32,
        step_name: String,
    },
    SagaCompleted {
        saga_id: String,
    },
    SignalReceived {
        name: String,
        payload: Vec<u8>,
    },
    TimerScheduled {
        name: String,
        fire_at_millis: u64,
    },
    TimerFired {
        name: String,
        fire_at_millis: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct WorkflowHistory {
    pub revision: u64,
    pub events: Vec<WorkflowEvent>,
}

impl WorkflowHistory {
    pub fn new(revision: u64, events: Vec<WorkflowEvent>) -> Self {
        Self { revision, events }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_activity_invocation_id_is_stable_and_fixed_width() {
        let workflow = WorkflowId::new("customer/order/42");
        let first = ActivityInvocationId::derive(&workflow, "charge", 0, "payments.charge");
        let second = ActivityInvocationId::derive(&workflow, "charge", 0, "payments.charge");

        assert_eq!(first, second);
        assert_eq!(first.to_string().len(), 64);
        assert_eq!(first.idempotency_key(), first.to_string());
    }

    #[test]
    fn test_activity_invocation_id_domain_separates_inputs() {
        let workflow = WorkflowId::new("ab");
        let other_workflow = WorkflowId::new("a");

        assert_ne!(
            ActivityInvocationId::derive(&workflow, "c", 0, "op"),
            ActivityInvocationId::derive(&other_workflow, "bc", 0, "op")
        );
        assert_ne!(
            ActivityInvocationId::derive(&workflow, "c", 0, "op"),
            ActivityInvocationId::derive(&workflow, "c", 1, "op")
        );
    }
}
