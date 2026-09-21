//! Core single-shard actor execution state.
//!
//! `Runtime` still composes distribution, persistence, host providers, web,
//! AI, and other optional subsystems. `RuntimeKernel` is the smaller state
//! boundary those systems operate on: actors, supervision, scheduling, and
//! actor-local memory/GC coordination.
//!
//! During the extraction, `Runtime` dereferences to this type so existing
//! runtime code can migrate incrementally without a flag-day rewrite. New
//! subsystems should prefer the narrowest kernel/subsystem interface rather
//! than adding more fields directly to `Runtime`.

use std::collections::HashMap;

use super::actor::Actor;
use super::gc::{OrcaCoordinator, OrcaGc};
use super::heap::ActorHeap;
use super::orca_cycle::CycleDetector;
use super::scheduler::Scheduler;
use super::supervisor::Supervisor;
use super::trace::TraceContext;
use super::MAIN_HEAP_ACTOR_ID;

/// Minimal actor-execution kernel owned by a runtime shard.
///
/// This deliberately excludes persistence, distribution, HTTP, AI, caches,
/// object storage, and other platform services. Those are composed by
/// `Runtime` around this kernel rather than becoming actor-execution
/// primitives.
pub struct RuntimeKernel {
    pub actors: HashMap<u64, Actor>,
    pub supervisors: HashMap<u64, Supervisor>,
    pub scheduler: Scheduler,
    pub current_actor: Option<u64>,
    /// W3C trace context for the message currently executing on this shard.
    pub current_trace: Option<TraceContext>,
    /// Fallback heap for top-level execution outside an actor behavior.
    pub main_heap: ActorHeap,
    pub main_gc: OrcaGc,
    /// Reduction budget assigned to the next scheduling round.
    pub next_reductions: u32,
    pub coordinator: OrcaCoordinator,
    pub cycle_detector: CycleDetector,
    /// Heaps whose actors exited while foreign references were still live.
    pub(super) retired_heaps: Vec<ActorHeap>,
}

impl RuntimeKernel {
    pub fn new() -> Self {
        let mut main_heap = ActorHeap::new(64 * 1024);
        main_heap.set_actor_id(MAIN_HEAP_ACTOR_ID);

        Self {
            actors: HashMap::new(),
            supervisors: HashMap::new(),
            scheduler: Scheduler::new(4),
            current_actor: None,
            current_trace: None,
            main_heap,
            main_gc: OrcaGc::new(MAIN_HEAP_ACTOR_ID),
            next_reductions: 1000,
            coordinator: OrcaCoordinator::new(),
            cycle_detector: CycleDetector::new(),
            retired_heaps: Vec::new(),
        }
    }
}

impl Default for RuntimeKernel {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_kernel_starts_quiescent() {
        let kernel = RuntimeKernel::new();
        assert!(kernel.actors.is_empty());
        assert!(kernel.supervisors.is_empty());
        assert!(kernel.current_actor.is_none());
        assert!(kernel.current_trace.is_none());
        assert!(kernel.retired_heaps.is_empty());
        assert_eq!(kernel.next_reductions, 1000);
    }
}
