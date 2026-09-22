//! Durable workflow orchestration primitives for Nulang Cloud SDKs.
//!
//! The crate intentionally depends on no compiler/runtime internals. Hosts
//! implement [`WorkflowRuntime`], while the executor owns replay-safe activity
//! identity, optimistic history concurrency, retry scheduling, and cached
//! completion replay.

mod coordination;
mod engine;
mod history;
mod lease;
mod retry;
mod saga;

pub use engine::{
    ActivityDispatchRequest, ActivityDispatchResult, ActivityProgress, ActivitySpec, AppendOutcome,
    DurableWorkflowExecutor, WorkflowEngineError, WorkflowRuntime,
};
pub use history::{
    ActivityInvocationId, SignalId, SignalWaitId, TimerId, WorkflowEvent, WorkflowHistory,
    WorkflowId,
};
pub use lease::{
    FenceCheck, LeaseAcquireOutcome, LeaseReleaseOutcome, LeaseRenewOutcome, WorkerLease,
    WorkerLeaseStore,
};
pub use retry::{Backoff, RetryPolicy};

pub use saga::{
    DurableSagaExecutor, SagaAction, SagaPhase, SagaPlan, SagaProgress, SagaStep,
};

pub use coordination::{
    DurableSignalExecutor, DurableTimerExecutor, SignalNotifyOutcome, SignalProgress,
    TimerArmRequest, TimerProgress, WorkflowTimerRuntime,
};
