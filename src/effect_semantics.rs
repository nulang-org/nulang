//! Compiler-owned effect execution semantics.
//!
//! Replay safety and concurrency safety are separate axes. Host effects reuse
//! the canonical replay classification in `host_effect_abi`; this module adds
//! the execution constraint needed by structured concurrency without teaching
//! `par` its own source-name policy.

use crate::host_effect_abi::{lookup_host_operation, HostReplayClass};

/// Effect-imposed scheduling constraint for a scoped concurrent branch.
///
/// This is intentionally conservative. `Unconstrained` means the effect itself
/// adds no scheduling restriction; capture/ownership analysis must still prove
/// that the branch can leave the owning actor thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ParallelEffectConstraint {
    #[default]
    Unconstrained,
    /// The operation is local but must remain on the owning runtime/actor
    /// thread until its implementation is proven worker-safe.
    ActorThreadOnly,
    /// Preserve source ordering. The operation is externally observable,
    /// blocking/suspending, or otherwise not yet safe to overlap.
    SequentialOnly,
}

impl ParallelEffectConstraint {
    pub const fn combine(self, other: Self) -> Self {
        use ParallelEffectConstraint::{ActorThreadOnly, SequentialOnly, Unconstrained};
        match (self, other) {
            (SequentialOnly, _) | (_, SequentialOnly) => SequentialOnly,
            (ActorThreadOnly, _) | (_, ActorThreadOnly) => ActorThreadOnly,
            (Unconstrained, Unconstrained) => Unconstrained,
        }
    }
}

/// Compiler-owned semantics for one performed effect operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EffectExecutionSemantics {
    pub parallel: ParallelEffectConstraint,
    /// Canonical durable-replay classification when this operation crosses the
    /// compiler-owned host ABI. Custom/local effects may have no host class.
    pub replay: Option<HostReplayClass>,
    /// The current implementation may suspend the executing computation.
    pub may_suspend: bool,
    /// Reordering with sibling branches can be externally observable.
    pub externally_observable: bool,
}

impl EffectExecutionSemantics {
    const fn new(
        parallel: ParallelEffectConstraint,
        replay: Option<HostReplayClass>,
        may_suspend: bool,
        externally_observable: bool,
    ) -> Self {
        Self {
            parallel,
            replay,
            may_suspend,
            externally_observable,
        }
    }
}

/// Classify a checked source-level effect operation.
///
/// Unknown/custom effects fail closed to `SequentialOnly` until their handler
/// contract has compiler-owned execution semantics. This does not reject the
/// source program: it only prevents a future `par` executor from silently
/// overlapping an operation whose ordering contract is unknown.
pub fn classify_effect_operation(effect: &str, operation: &str) -> EffectExecutionSemantics {
    if let Some(host) = lookup_host_operation(effect, operation) {
        let parallel = match host.replay {
            HostReplayClass::Pure => ParallelEffectConstraint::Unconstrained,
            HostReplayClass::LocalReplaySafe => ParallelEffectConstraint::ActorThreadOnly,
            HostReplayClass::JournalResult
            | HostReplayClass::ExternalIdempotent
            | HostReplayClass::ExternalRequiresIdempotencyKey
            | HostReplayClass::ExternalNonreplayable => ParallelEffectConstraint::SequentialOnly,
        };
        let may_suspend = matches!(
            (effect, operation),
            ("Inference", "ask") | ("Queue", "pop") | ("Timer", "sleep")
        );
        return EffectExecutionSemantics::new(
            parallel,
            Some(host.replay),
            may_suspend,
            !matches!(host.replay, HostReplayClass::Pure | HostReplayClass::LocalReplaySafe),
        );
    }

    match (effect, operation) {
        // Runtime-local value construction. Keep it on the owning thread until
        // its heap/constant-pool implementation is explicitly worker-safe.
        ("String", "from_char") => EffectExecutionSemantics::new(
            ParallelEffectConstraint::ActorThreadOnly,
            None,
            false,
            false,
        ),

        // Suspension points require scoped continuation/cancellation machinery.
        ("Signal", "wait") => EffectExecutionSemantics::new(
            ParallelEffectConstraint::SequentialOnly,
            None,
            true,
            false,
        ),

        // Actor lifecycle/messaging, domain-event emission, ambient I/O/time,
        // randomness, process/FFI and unregistered host effects all have
        // observable ordering or runtime affinity today.
        ("Actor", _)
        | ("Otp", _)
        | ("Event", _)
        | ("IO", _)
        | ("FS", _)
        | ("Net", _)
        | ("Rand", _)
        | ("Time", _)
        | ("Process", _)
        | ("DB", _)
        | ("Python", _)
        | ("Http", _)
        | ("Storage", _)
        | ("Queue", _)
        | ("Comms", _)
        | ("Agent", _)
        | ("Inference", _)
        | ("Timer", _) => EffectExecutionSemantics::new(
            ParallelEffectConstraint::SequentialOnly,
            None,
            matches!((effect, operation), ("Actor", "receive") | ("Signal", "wait") | ("Timer", "sleep")),
            true,
        ),

        // Custom effects require an explicit compiler-owned contract before a
        // concurrent executor may overlap them.
        _ => EffectExecutionSemantics::new(
            ParallelEffectConstraint::SequentialOnly,
            None,
            false,
            true,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_replay_contract_is_reused_for_parallel_classification() {
        let semantics = classify_effect_operation("Storage", "read");
        assert_eq!(semantics.replay, Some(HostReplayClass::JournalResult));
        assert_eq!(
            semantics.parallel,
            ParallelEffectConstraint::SequentialOnly
        );
        assert!(semantics.externally_observable);
    }

    #[test]
    fn suspension_points_are_never_worker_parallel_by_default() {
        for (effect, operation) in [
            ("Timer", "sleep"),
            ("Signal", "wait"),
            ("Inference", "ask"),
        ] {
            let semantics = classify_effect_operation(effect, operation);
            assert_eq!(
                semantics.parallel,
                ParallelEffectConstraint::SequentialOnly
            );
            assert!(semantics.may_suspend);
        }
    }

    #[test]
    fn local_heap_operation_is_actor_thread_affine() {
        let semantics = classify_effect_operation("String", "from_char");
        assert_eq!(
            semantics.parallel,
            ParallelEffectConstraint::ActorThreadOnly
        );
        assert!(!semantics.externally_observable);
    }

    #[test]
    fn unknown_custom_effect_fails_closed_for_overlap() {
        let semantics = classify_effect_operation("MyEffect", "run");
        assert_eq!(
            semantics.parallel,
            ParallelEffectConstraint::SequentialOnly
        );
        assert_eq!(semantics.replay, None);
    }
}
