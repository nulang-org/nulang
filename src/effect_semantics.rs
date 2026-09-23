//! Shared semantic classification for effect operations.
//!
//! This module deliberately stays conservative. Unknown user-defined effects
//! remain `Unknown` until the compiler can prove stronger properties from
//! declarations/handlers. The same metadata is intended to feed structured
//! concurrency, durable replay, and future optimizer legality checks.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConcurrencySemantics {
    /// The operation may execute concurrently without an ordering contract.
    Concurrent,
    /// The operation must retain an explicit ordering/serialization boundary.
    Serialized,
    /// The compiler does not yet know enough to permit concurrent execution.
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplaySemantics {
    /// Re-executing the operation with the same inputs is semantically stable.
    Deterministic,
    /// The result must be captured/journaled if deterministic replay is needed.
    CaptureResult,
    /// Re-execution requires a protocol such as journaling/idempotency/fencing.
    RequiresProtocol,
    /// Replay behavior is not known statically.
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdempotencySemantics {
    Idempotent,
    NonIdempotent,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancellationSemantics {
    /// Cancellation may stop the operation without leaving an external commit.
    Immediate,
    /// Cancellation must wait for a defined semantic boundary/cleanup point.
    Deferred,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EffectOperationSemantics {
    pub concurrency: ConcurrencySemantics,
    pub replay: ReplaySemantics,
    pub idempotency: IdempotencySemantics,
    pub cancellation: CancellationSemantics,
}

impl EffectOperationSemantics {
    pub const UNKNOWN: Self = Self {
        concurrency: ConcurrencySemantics::Unknown,
        replay: ReplaySemantics::Unknown,
        idempotency: IdempotencySemantics::Unknown,
        cancellation: CancellationSemantics::Unknown,
    };

    pub const PURE_INTRINSIC: Self = Self {
        concurrency: ConcurrencySemantics::Concurrent,
        replay: ReplaySemantics::Deterministic,
        idempotency: IdempotencySemantics::Idempotent,
        cancellation: CancellationSemantics::Immediate,
    };

    pub const ORDERED_EXTERNAL: Self = Self {
        concurrency: ConcurrencySemantics::Serialized,
        replay: ReplaySemantics::RequiresProtocol,
        idempotency: IdempotencySemantics::Unknown,
        cancellation: CancellationSemantics::Deferred,
    };

    pub const CAPTURED_NONDETERMINISM: Self = Self {
        concurrency: ConcurrencySemantics::Concurrent,
        replay: ReplaySemantics::CaptureResult,
        idempotency: IdempotencySemantics::NonIdempotent,
        cancellation: CancellationSemantics::Immediate,
    };
}

/// Classify the flattened operation names currently emitted by source-level
/// parallel analysis.
///
/// This is intentionally a compatibility adapter. Typed HIR/MIR should
/// eventually carry structured effect identity directly rather than relying on
/// strings, but keeping the policy here prevents multiple subsystems from
/// inventing their own ad-hoc classifications in the meantime.
pub fn classify_operation_name(name: &str) -> EffectOperationSemantics {
    match name {
        // Compiler-local metadata, not an observable host interaction.
        "Closure.capture" => EffectOperationSemantics::PURE_INTRINSIC,

        // A dynamic call needs the callee's inferred/transitive effect summary.
        "Call" => EffectOperationSemantics::UNKNOWN,

        // Actor/event primitives are ordered semantic boundaries. A future
        // durable host may journal/fence them, but they are never safe to
        // blindly replay or reorder based only on the source operation name.
        "Actor.spawn" | "Actor.receive" | "Actor.migrate" | "Grain.ref" => {
            EffectOperationSemantics::ORDERED_EXTERNAL
        }
        _ if name.starts_with("Actor.send.")
            || name.starts_with("Actor.ask.")
            || name.starts_with("Event.emit.") =>
        {
            EffectOperationSemantics::ORDERED_EXTERNAL
        }

        // Known pure VM intrinsics. Keep this whitelist deliberately small.
        "String.length" | "String.charAt" | "Array.length" | "Int.to_string"
        | "Int.to_float" => EffectOperationSemantics::PURE_INTRINSIC,

        // Reading time/randomness is safe to overlap from a memory-ordering
        // perspective, but deterministic replay must capture the observed value.
        _ if name.starts_with("Rand.")
            || name.starts_with("Random.")
            || name == "Time.now"
            || name == "Time.monotonic" =>
        {
            EffectOperationSemantics::CAPTURED_NONDETERMINISM
        }

        _ => EffectOperationSemantics::UNKNOWN,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pure_intrinsics_are_parallel_and_replay_deterministic() {
        let semantics = classify_operation_name("String.length");
        assert_eq!(semantics.concurrency, ConcurrencySemantics::Concurrent);
        assert_eq!(semantics.replay, ReplaySemantics::Deterministic);
        assert_eq!(semantics.idempotency, IdempotencySemantics::Idempotent);
    }

    #[test]
    fn actor_send_requires_ordered_protocol() {
        let semantics = classify_operation_name("Actor.send.deliver");
        assert_eq!(semantics.concurrency, ConcurrencySemantics::Serialized);
        assert_eq!(semantics.replay, ReplaySemantics::RequiresProtocol);
        assert_eq!(semantics.cancellation, CancellationSemantics::Deferred);
    }

    #[test]
    fn unknown_user_effects_fail_closed() {
        assert_eq!(
            classify_operation_name("Payments.charge"),
            EffectOperationSemantics::UNKNOWN
        );
    }

    #[test]
    fn time_observations_require_result_capture_for_replay() {
        let semantics = classify_operation_name("Time.now");
        assert_eq!(semantics.concurrency, ConcurrencySemantics::Concurrent);
        assert_eq!(semantics.replay, ReplaySemantics::CaptureResult);
        assert_eq!(
            semantics.idempotency,
            IdempotencySemantics::NonIdempotent
        );
    }
}
