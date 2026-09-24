#![forbid(unsafe_code)]

//! Nulang Cloud control-plane primitives.
//!
//! This crate intentionally contains no provider SDKs, network servers, or runtime
//! mutation. It turns desired deployment state plus an observed cluster snapshot
//! into a deterministic placement plan that can be persisted, validated, fenced,
//! and then committed by a higher-level reconciler.

pub mod executor;
pub mod model;
pub mod node_executor;
pub mod reconciler;
pub mod scheduler;
pub mod store;

pub use executor::{
    dispatch_pending, dispatch_pending_as, AllocationCommandSink, CommandApplyError,
    DispatchOutcome, DispatchRecord, DispatchReport,
};
pub use model::*;
pub use node_executor::{
    FencedNodeExecutor, NodeAllocationPhase, NodeAllocationRecord, WorkloadLifecycle,
};
pub use reconciler::{reconcile_once, ReconcileError, ReconcileResult};
pub use scheduler::plan_evaluation;
pub use store::*;
