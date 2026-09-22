use crate::RetryPolicy;
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
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

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ActivityInvocationId(String);

impl ActivityInvocationId {
    pub fn derive(
        workflow_id: &WorkflowId,
        step: &str,
        occurrence: u32,
        operation: &str,
    ) -> Self {
        fn part(out: &mut String, label: &str, value: &str) {
            out.push_str(label);
            out.push_str(&value.len().to_string());
            out.push(':');
            out.push_str(value);
            out.push('|');
        }

        let mut value = String::from("nulang.activity.v1|");
        part(&mut value, "wf", workflow_id.as_str());
        part(&mut value, "step", step);
        value.push_str("occ");
        value.push_str(&occurrence.to_string());
        value.push('|');
        part(&mut value, "op", operation);
        Self(value)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn idempotency_key(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ActivityInvocationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
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
    },
    ActivityRetryScheduled {
        invocation_id: ActivityInvocationId,
        next_attempt: u32,
        ready_at_millis: u64,
    },
    ActivityCompleted {
        invocation_id: ActivityInvocationId,
        result: Vec<u8>,
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

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WorkflowHistory {
    pub revision: u64,
    pub events: Vec<WorkflowEvent>,
}

impl WorkflowHistory {
    pub fn new(revision: u64, events: Vec<WorkflowEvent>) -> Self {
        Self { revision, events }
    }
}
