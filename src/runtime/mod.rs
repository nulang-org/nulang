//! Actor runtime system for Nulang.
//!
//! Provides: actor lifecycle, scheduler, mailbox, heap, GC, supervision,
//! distribution.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Instant;
use tracing::warn;

mod actor;
mod gc;
pub mod heap;
pub(crate) mod heap_serialize;
mod mailbox;
mod scheduler;
pub use heap_serialize::*;
mod cluster;
mod distributed;
mod distributed_context;
mod grain;
mod network;
mod object_store;
mod orca_cycle;
mod supervision;
mod supervisor;
use distributed_context::DistributedContext;
#[cfg(feature = "ai-runtime")]
mod agent;
#[cfg(feature = "ai-runtime")]
mod ai_impls;
pub(crate) mod callbacks;
pub mod crdt;
pub mod crdt_manager;
pub mod crdt_reg;
mod distribution;
mod exit;
mod http_server;
#[cfg(feature = "ai-runtime")]
mod llm;
mod metrics;
mod persistence;
mod process_groups;
mod registry;
mod spawn;
mod timer;
mod trace;
mod workflow;
pub use trace::TraceContext;

#[cfg(test)]
mod cluster_dst;

#[cfg(test)]
mod cluster_sim;

#[cfg(test)]
mod tests;

pub use actor::*;
pub use callbacks::RuntimeVmCallbacks;
pub(crate) use callbacks::{BytecodeDistributedCallbacks, BytecodeRuntimeCallbacks};
pub use cluster::*;
pub use crdt::*;
pub use crdt_manager::*;
pub use crdt_reg::{LWWRegister, MVRegister, RGAElement, RGA};
pub use distributed::*;
pub use gc::{ForeignRefOp, GcStats, OrcaCoordinator, OrcaGc, OrcaHeap};
pub use grain::*;
pub use heap::*;
pub use http_server::{
    render_route_handler, HttpMethod, HttpServerState, WebDevServer, WebRoute,
};
pub use mailbox::*;
pub use network::NetworkTransport;
pub use network::*;
pub use object_store::*;
pub use orca_cycle::*;
pub use persistence::*;
pub use process_groups::*;
pub use registry::*;
pub use scheduler::*;
pub use supervisor::*;
pub use timer::*;

use crate::types::{ExitReason, NuError, Span, VmSuspension};
use crate::vm::Value;

#[cfg(feature = "ai-runtime")]
use nulang_ai::{
    self, AgentError, AgentExecutor, AgentValue, AiRuntime, Directive, ProviderConfig,
    ProviderRegistry, TaskEnvelope, ToolRegistry, WorkerRole,
};
