//! Canonical semantic vocabulary for Nulang execution.
//!
//! Nulang has one semantic core and multiple composable execution forms.
//! Ordinary local computation, scoped concurrent tasks, and actors are distinct
//! execution domains. Persistence and identity are orthogonal properties rather
//! than actor species.
//!
//! The existing [`ActorRole`] remains a compatibility boundary while older
//! compiler/runtime metadata still uses mutually-exclusive boolean role flags.
//! It describes source/lowering provenance for actor-backed constructs; it must
//! not be used as the complete semantic model for new features.

/// The execution domain in which a computation runs.
///
/// This axis deliberately does not encode durability, identity, placement, or
/// authority. Those properties compose independently.
///
/// `ScopedTask` names the semantic target for structured concurrency. The
/// current `par { ... }` implementation is still sequential until the scoped
/// concurrency lowering described by RFC 0024 is implemented.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExecutionDomain {
    /// Ordinary lexical computation: functions, expressions, and optimized
    /// local numeric/data-parallel code.
    Local,
    /// Child computation whose lifetime is bounded by a lexical parent scope.
    ScopedTask,
    /// Independently addressable isolated state with mailbox/turn semantics.
    Actor,
}

/// Persistence semantics are orthogonal to the execution domain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PersistenceSemantics {
    /// State/computation disappears when its owning execution lifetime ends.
    Ephemeral,
    /// Runtime-managed state/history survives restart.
    Durable,
    /// An append-only domain-event history is the source of truth.
    EventSourced,
    /// State is replicated/merged according to an explicit convergence model.
    Replicated,
}

/// Identity semantics are orthogonal to both execution and persistence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IdentitySemantics {
    /// No independently addressable identity.
    Anonymous,
    /// Identity exists only within the owning structured-concurrency scope.
    Scoped,
    /// Runtime identity exists for the lifetime of the live computation.
    Runtime,
    /// Stable logical identity may survive activation, restart, or placement.
    Stable,
}

/// Activation is actor-specific policy, not an actor role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ActorActivation {
    /// The actor/entity is explicitly created or restored by the runtime.
    Explicit,
    /// A stable key may transparently hydrate/dehydrate the actor on demand.
    Virtual,
}

/// Minimal orthogonal execution profile.
///
/// This type is intentionally representation-agnostic and is not serialized in
/// bytecode yet. It gives compiler/runtime code a vocabulary that does not
/// conflate "actor", "durable", and "stable identity" into one role enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ExecutionSemantics {
    pub domain: ExecutionDomain,
    pub persistence: PersistenceSemantics,
    pub identity: IdentitySemantics,
}

impl ExecutionSemantics {
    pub const fn new(
        domain: ExecutionDomain,
        persistence: PersistenceSemantics,
        identity: IdentitySemantics,
    ) -> Self {
        Self {
            domain,
            persistence,
            identity,
        }
    }

    pub const LOCAL: Self = Self::new(
        ExecutionDomain::Local,
        PersistenceSemantics::Ephemeral,
        IdentitySemantics::Anonymous,
    );

    pub const SCOPED_TASK: Self = Self::new(
        ExecutionDomain::ScopedTask,
        PersistenceSemantics::Ephemeral,
        IdentitySemantics::Scoped,
    );

    pub const ACTOR: Self = Self::new(
        ExecutionDomain::Actor,
        PersistenceSemantics::Ephemeral,
        IdentitySemantics::Runtime,
    );
}

/// Legacy actor-runtime primitive inventory introduced by RFC 0017.
///
/// These names remain useful for decomposing actor runtime internals, but they
/// are not the complete ontology of the language. In particular, local
/// computation and scoped tasks are execution domains in their own right.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RuntimePrimitive {
    Actor,
    State,
    Message,
    Effect,
    /// Reference capability / aliasing and sendability semantics.
    Capability,
    /// External permission to cross a host/security boundary.
    Authority,
    Supervisor,
    Time,
}

/// Legacy compatibility classifier carried by actor-backed lowered forms.
///
/// `Agent`, `Workflow`, `Organization`, and `Virtual` describe how older
/// metadata produced an actor. They are not intended to be mutually-exclusive
/// semantic dimensions in the long-term model: for example, virtuality is an
/// [`ActorActivation`] policy, while workflow durability belongs on the
/// persistence/execution axes. New features should prefer explicit semantic
/// properties and use this enum only while compatibility flags are migrated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ActorRole {
    Plain,
    Agent,
    Workflow,
    Organization,
    Virtual,
}

impl ActorRole {
    /// Derive one canonical role from the legacy role flags.
    ///
    /// Multiple role flags represent invalid metadata. Keeping this check in a
    /// single place prevents subtle precedence differences between compiler
    /// and runtime subsystems while the flags are phased out.
    pub fn from_flags(
        is_workflow: bool,
        is_agent: bool,
        is_organization: bool,
        is_virtual: bool,
    ) -> Result<Self, ActorRoleConflict> {
        let roles = [
            (is_workflow, Self::Workflow),
            (is_agent, Self::Agent),
            (is_organization, Self::Organization),
            (is_virtual, Self::Virtual),
        ];

        let mut selected = None;
        let mut count = 0usize;
        for (enabled, role) in roles {
            if enabled {
                count += 1;
                selected = Some(role);
            }
        }

        if count > 1 {
            return Err(ActorRoleConflict {
                is_workflow,
                is_agent,
                is_organization,
                is_virtual,
            });
        }

        Ok(selected.unwrap_or(Self::Plain))
    }

    /// Whether this role is surface sugar over the actor runtime.
    pub const fn is_composite_surface(self) -> bool {
        !matches!(self, Self::Plain)
    }
}

/// Invalid legacy actor metadata with more than one specialized role.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActorRoleConflict {
    pub is_workflow: bool,
    pub is_agent: bool,
    pub is_organization: bool,
    pub is_virtual: bool,
}

impl std::fmt::Display for ActorRoleConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "actor metadata has conflicting roles (workflow={}, agent={}, organization={}, virtual={})",
            self.is_workflow,
            self.is_agent,
            self.is_organization,
            self.is_virtual
        )
    }
}

impl std::error::Error for ActorRoleConflict {}

impl crate::hir::ActorDef {
    /// Return the legacy compatibility role of this lowered actor.
    ///
    /// New HIR consumers should prefer this helper to testing the legacy flags
    /// separately. Once all consumers use it, the flags can be replaced by a
    /// single role field without changing source-language semantics.
    pub fn role(&self) -> Result<ActorRole, ActorRoleConflict> {
        ActorRole::from_flags(
            self.is_workflow,
            self.is_agent,
            self.is_organization,
            self.virtual_,
        )
    }
}

impl crate::bytecode::ActorMeta {
    /// Return the compatibility role encoded in serialized actor metadata without
    /// changing the bytecode format.
    ///
    /// Keeping this derivation identical to HIR is the first migration step
    /// toward replacing four serialized booleans with one versioned role
    /// field in a future format revision.
    pub fn role(&self) -> Result<ActorRole, ActorRoleConflict> {
        ActorRole::from_flags(
            self.is_workflow,
            self.is_agent,
            self.is_organization,
            self.is_virtual,
        )
    }
}

impl crate::runtime::Actor {
    /// Return the compatibility role of a live runtime actor.
    ///
    /// Runtime actors currently persist only the legacy workflow/agent flags;
    /// organization and virtual status are compiler/placement metadata. Keeping
    /// role interpretation here makes runtime subsystems consume the same
    /// semantic model as HIR and bytecode without changing the persisted actor
    /// representation in this phase.
    pub fn role(&self) -> Result<ActorRole, ActorRoleConflict> {
        ActorRole::from_flags(self.is_workflow, self.is_agent, false, false)
    }
}

/// Runtime operations implemented by the single [`RuntimePrimitive::Time`]
/// primitive.
///
/// The timer wheel has multiple internal wake-message variants for efficiency,
/// but those variants are implementation detail. Language features such as
/// `Timer.sleep`, receive deadlines, workflow timers, delayed delivery, and
/// retry backoff all share this semantic primitive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TimeOperation {
    /// Resume computation after an explicit sleep.
    Sleep,
    /// Deliver a message after a delay, including durable workflow timers.
    ScheduledDelivery,
    /// Enforce a timeout/deadline or delayed termination.
    Deadline,
    /// Wake a retry attempt after backoff.
    RetryBackoff,
}

impl crate::runtime::TimerMessage {
    /// Classify an internal timer-wheel message as a canonical Time operation.
    pub fn time_operation(&self) -> TimeOperation {
        match self {
            Self::TimerSleepWake => TimeOperation::Sleep,
            Self::Send { .. } | Self::SendWithContext { .. } => TimeOperation::ScheduledDelivery,
            Self::Exit { .. } | Self::Kill | Self::ReceiveWaitTimeout => TimeOperation::Deadline,
            Self::LlmRetry => TimeOperation::RetryBackoff,
        }
    }
}


/// Execution class for a performed effect operation.
///
/// This is runtime scheduling metadata, not source-language syntax. It lets the
/// runtime and Nulang Cloud decide whether an operation is safe on the actor
/// scheduler or must suspend/offload without baking transient executor choices
/// into the language.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EffectExecutionClass {
    /// Cheap, non-blocking work safe to execute on the actor scheduler.
    Inline,
    /// The actor should suspend while runtime-owned state/time/message progress
    /// determines when it can resume.
    CooperativeSuspend,
    /// External asynchronous work. The actor should suspend and the scheduler
    /// thread must remain free while the operation is in flight.
    AsyncExternal,
    /// Host work that may block an OS thread (filesystem, process, Python/FFI,
    /// terminal I/O). It must not run on the actor scheduler once isolation is
    /// wired.
    BlockingHost,
    /// Long-running CPU work that should execute on a compute pool rather than
    /// monopolizing a cooperative actor scheduler thread.
    CpuBound,
    /// Accelerator-backed work whose execution belongs on a GPU/TPU/etc.
    /// resource executor and normally suspends the actor.
    Accelerator,
    /// User-defined/unknown effect. Its handler owns the execution contract;
    /// callers must not assume scheduler safety.
    HandlerDefined,
}

impl EffectExecutionClass {
    /// Whether running the operation directly on an actor scheduler may stall
    /// unrelated actors.
    pub const fn requires_isolation(self) -> bool {
        matches!(
            self,
            Self::BlockingHost | Self::CpuBound | Self::Accelerator
        )
    }

    /// Whether normal actor execution should resume only after an asynchronous
    /// completion/wakeup.
    pub const fn suspends_actor(self) -> bool {
        matches!(
            self,
            Self::CooperativeSuspend | Self::AsyncExternal | Self::Accelerator
        )
    }
}

/// Classify a semantic effect plus operation name.
///
/// Operation-level overrides matter for effects such as Time: reading the
/// clock is inline, while sleeping is a cooperative suspension.
pub fn classify_effect_execution(
    effect: &crate::types::Effect,
    op: &str,
) -> EffectExecutionClass {
    use crate::types::Effect;
    use EffectExecutionClass::*;

    match (effect, op) {
        (Effect::Time, "sleep") | (Effect::Receive, _) => CooperativeSuspend,

        (Effect::Net, _)
        | (Effect::Migrate, _)
        | (Effect::Async, _)
        | (Effect::Inference, _)
        | (Effect::DB, _)
        | (Effect::Realtime, _) => AsyncExternal,

        (Effect::IO, _)
        | (Effect::FS, _)
        | (Effect::FFI, _)
        | (Effect::Python, _)
        | (Effect::Process, _)
        | (Effect::System, _) => BlockingHost,

        (Effect::UserDefined(_), _) => HandlerDefined,

        (Effect::String, _)
        | (Effect::Rand, _)
        | (Effect::Time, _)
        | (Effect::Spawn, _)
        | (Effect::Send, _)
        | (Effect::STM, _)
        | (Effect::Cost, _)
        | (Effect::Event, _)
        | (Effect::Array, _)
        | (Effect::Test, _)
        | (Effect::Env, _)
        | (Effect::Render, _)
        | (Effect::Request, _)
        | (Effect::Respond, _)
        | (Effect::Client, _)
        | (Effect::Web, _) => Inline,
    }
}

/// Classify the concrete named effects used by VM/runtime built-ins.
///
/// This covers aliases/runtime surfaces that are intentionally not distinct
/// variants in the stable Effect enum (Http→Net, Inference/LLM, Timer, Signal,
/// Actor/Otp/Crdt, and pure helper namespaces). Unknown names fail closed as
/// HandlerDefined so adding a built-in requires an explicit scheduling choice.
pub fn classify_named_effect_execution(effect: &str, op: &str) -> EffectExecutionClass {
    use EffectExecutionClass::*;

    match (effect, op) {
        ("Timer", "sleep") | ("Time", "sleep") | ("Signal", "wait") => CooperativeSuspend,

        ("Http", _)
        | ("Net", _)
        | ("Inference", _)
        | ("LLM", _)
        | ("Realtime", _)
        | ("Database", _)
        | ("DB", _) => AsyncExternal,

        ("IO", _)
        | ("Debug", _)
        | ("FS", _)
        | ("Python", _)
        | ("FFI", _)
        | ("Process", _)
        | ("System", _) => BlockingHost,

        ("Actor", _)
        | ("Array", _)
        | ("Crdt", _)
        | ("Env", _)
        | ("Float", _)
        | ("Int", _)
        | ("Map", _)
        | ("Otp", _)
        | ("Random", _)
        | ("Signal", _)
        | ("StrBuilder", _)
        | ("String", _)
        | ("Test", _)
        | ("Time", _)
        | ("Timer", _)
        | ("Web", _) => Inline,

        _ => HandlerDefined,
    }
}

/// Durability boundary for a side effect.
///
/// This is deliberately narrower than an "exactly once" claim. Nulang can
/// make its own journal/state transition atomic, but an arbitrary external
/// system cannot be made exactly-once without cooperation such as an
/// idempotency key or a distributed transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EffectBoundary {
    /// State owned by the Nulang runtime and committed with its journal.
    RuntimeOwned,
    /// State owned by a configured storage/queue backend; guarantees are
    /// defined by that backend's contract.
    BackendOwned,
    /// A side effect in an external system such as an HTTP API or model
    /// provider. Recovery may retry the operation.
    External,
}

/// Delivery/replay semantics that may be advertised by Nulang components.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeliverySemantics {
    /// The operation can be retried and therefore may be observed more than
    /// once by a receiver or external dependency.
    AtLeastOnce,
    /// Replays are deduplicated using a stable operation/message key.
    EffectivelyOnceWithDeduplication,
    /// The guarantee is delegated to a configured backend and must not be
    /// strengthened by the language/runtime documentation.
    BackendDefined,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn execution_domain_is_independent_from_persistence_and_identity() {
        let ephemeral_actor = ExecutionSemantics::ACTOR;
        let durable_entity = ExecutionSemantics::new(
            ExecutionDomain::Actor,
            PersistenceSemantics::Durable,
            IdentitySemantics::Stable,
        );
        let event_sourced_entity = ExecutionSemantics::new(
            ExecutionDomain::Actor,
            PersistenceSemantics::EventSourced,
            IdentitySemantics::Stable,
        );

        assert_eq!(ephemeral_actor.domain, durable_entity.domain);
        assert_ne!(ephemeral_actor.persistence, durable_entity.persistence);
        assert_ne!(durable_entity.persistence, event_sourced_entity.persistence);
        assert_eq!(durable_entity.identity, IdentitySemantics::Stable);
    }

    #[test]
    fn scoped_tasks_are_not_actor_roles() {
        assert_eq!(
            ExecutionSemantics::SCOPED_TASK.domain,
            ExecutionDomain::ScopedTask
        );
        assert_eq!(
            ExecutionSemantics::SCOPED_TASK.persistence,
            PersistenceSemantics::Ephemeral
        );
        assert_eq!(
            ExecutionSemantics::SCOPED_TASK.identity,
            IdentitySemantics::Scoped
        );
    }

    #[test]
    fn plain_actor_has_plain_role() {
        assert_eq!(
            ActorRole::from_flags(false, false, false, false),
            Ok(ActorRole::Plain)
        );
    }

    #[test]
    fn specialized_surface_maps_to_single_actor_role() {
        assert_eq!(
            ActorRole::from_flags(false, true, false, false),
            Ok(ActorRole::Agent)
        );
        assert_eq!(
            ActorRole::from_flags(true, false, false, false),
            Ok(ActorRole::Workflow)
        );
    }

    #[test]
    fn conflicting_roles_are_rejected() {
        assert!(ActorRole::from_flags(true, true, false, false).is_err());
        assert!(ActorRole::from_flags(false, true, true, false).is_err());
    }

    #[test]
    fn bytecode_metadata_uses_the_same_role_rules() {
        let mut meta = crate::bytecode::ActorMeta::new("assistant");
        meta.is_agent = true;
        assert_eq!(meta.role(), Ok(ActorRole::Agent));

        meta.is_workflow = true;
        assert!(meta.role().is_err());
    }

    #[test]
    fn live_runtime_actor_uses_the_same_role_rules() {
        let mut actor = crate::runtime::Actor::new(1, "order-workflow", 0);
        actor.is_workflow = true;
        assert_eq!(actor.role(), Ok(ActorRole::Workflow));

        actor.is_agent = true;
        assert!(actor.role().is_err());
    }

    #[test]
    fn effect_execution_class_distinguishes_waits_external_and_blocking_work() {
        use crate::types::Effect;

        assert_eq!(
            classify_effect_execution(&Effect::Time, "now"),
            EffectExecutionClass::Inline
        );
        assert_eq!(
            classify_effect_execution(&Effect::Time, "sleep"),
            EffectExecutionClass::CooperativeSuspend
        );
        assert_eq!(
            classify_effect_execution(&Effect::Inference, "ask"),
            EffectExecutionClass::AsyncExternal
        );
        assert_eq!(
            classify_effect_execution(&Effect::Python, "call"),
            EffectExecutionClass::BlockingHost
        );
        assert_eq!(
            classify_effect_execution(&Effect::UserDefined("Custom".into()), "run"),
            EffectExecutionClass::HandlerDefined
        );
    }

    #[test]
    fn execution_class_helpers_expose_scheduler_contract() {
        assert!(EffectExecutionClass::BlockingHost.requires_isolation());
        assert!(EffectExecutionClass::CpuBound.requires_isolation());
        assert!(EffectExecutionClass::Accelerator.requires_isolation());
        assert!(!EffectExecutionClass::Inline.requires_isolation());

        assert!(EffectExecutionClass::CooperativeSuspend.suspends_actor());
        assert!(EffectExecutionClass::AsyncExternal.suspends_actor());
        assert!(EffectExecutionClass::Accelerator.suspends_actor());
        assert!(!EffectExecutionClass::BlockingHost.suspends_actor());
    }

    #[test]
    fn internal_timer_messages_lower_to_time_operations() {
        use crate::runtime::TimerMessage;

        assert_eq!(
            TimerMessage::TimerSleepWake.time_operation(),
            TimeOperation::Sleep
        );
        assert_eq!(
            TimerMessage::ReceiveWaitTimeout.time_operation(),
            TimeOperation::Deadline
        );
        assert_eq!(
            TimerMessage::LlmRetry.time_operation(),
            TimeOperation::RetryBackoff
        );
        assert_eq!(
            TimerMessage::Send {
                behavior_id: 0,
                payload: vec![],
            }
            .time_operation(),
            TimeOperation::ScheduledDelivery
        );
    }
}
