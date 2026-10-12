//! Durable workflow orchestration primitives for Nulang Cloud SDKs.
//!
//! The crate intentionally depends on no compiler/runtime internals. Hosts
//! implement [`WorkflowRuntime`], while the executor owns replay-safe activity
//! identity, optimistic history concurrency, retry scheduling, and cached
//! completion replay.

mod engine;
mod history;
mod retry;

pub use engine::{
    ActivityDispatchRequest, ActivityDispatchResult, ActivityProgress, ActivitySpec, AppendOutcome,
    DurableWorkflowExecutor, WorkflowEngineError, WorkflowRuntime,
};
pub use history::{ActivityInvocationId, WorkflowEvent, WorkflowHistory, WorkflowId};
pub use retry::{Backoff, RetryPolicy};
