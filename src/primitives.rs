//! Canonical semantic primitives for the Nulang runtime.
//!
//! Nulang intentionally keeps the runtime model smaller than the surface
//! syntax. `agent`, `workflow`, `entity`, `organization`, and virtual-actor
//! syntax are compositions or specializations of actors; they are not
//! independent execution species.
//!
//! This module is the compatibility boundary while older metadata still uses
//! boolean role flags. New compiler/runtime code should ask for
//! [`ActorSemantics`] instead of branching independently on `is_agent`,
//! `is_workflow`, `is_organization`, and `virtual_`. [`ActorRole`] remains
//! as a compatibility view for persisted formats while those flags are phased out.

/// The seven semantic primitives that make up the Nulang execution model.
///
/// Higher-level features should lower to compositions of these primitives
/// instead of introducing additional runtime species.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RuntimePrimitive {
    Actor,
    State,
    Message,
    Effect,
    Capability,
    Supervisor,
    Time,
}


/// Whether an actor's state survives runtime/process failure.
///
/// Durability is orthogonal to why an actor was created. An agent, workflow,
/// organization, or plain actor may all be durable without becoming a new
/// execution species.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ActorDurability {
    Transient,
    Durable,
}

/// How an actor instance is activated.
///
/// Virtual activation is a placement/lifecycle policy, not a mutually exclusive
/// actor role. Keeping it separate allows future combinations such as virtual
/// agents or workflows without adding another runtime species.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ActorActivation {
    Eager,
    Virtual,
}

/// Source-level construct that produced an actor after lowering.
///
/// This is provenance for diagnostics and compatibility, not an execution model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ActorSurfaceOrigin {
    Actor,
    Agent,
    Workflow,
    Organization,
}

/// Canonical orthogonal semantics of a lowered actor.
///
/// Legacy metadata stores these dimensions as several booleans. This structure
/// is the normalization boundary new compiler/runtime code should consume until
/// the serialized format can replace those booleans directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ActorSemantics {
    pub durability: ActorDurability,
    pub activation: ActorActivation,
    pub origin: ActorSurfaceOrigin,
}

impl ActorSemantics {
    pub fn from_legacy_flags(
        persistent: bool,
        is_workflow: bool,
        is_agent: bool,
        is_organization: bool,
        is_virtual: bool,
    ) -> Result<Self, ActorOriginConflict> {
        let origins = [
            (is_workflow, ActorSurfaceOrigin::Workflow),
            (is_agent, ActorSurfaceOrigin::Agent),
            (is_organization, ActorSurfaceOrigin::Organization),
        ];

        let mut origin = ActorSurfaceOrigin::Actor;
        let mut count = 0usize;
        for (enabled, candidate) in origins {
            if enabled {
                count += 1;
                origin = candidate;
            }
        }

        if count > 1 {
            return Err(ActorOriginConflict {
                is_workflow,
                is_agent,
                is_organization,
            });
        }

        Ok(Self {
            durability: if persistent {
                ActorDurability::Durable
            } else {
                ActorDurability::Transient
            },
            activation: if is_virtual {
                ActorActivation::Virtual
            } else {
                ActorActivation::Eager
            },
            origin,
        })
    }

    pub const fn is_agent(self) -> bool {
        matches!(self.origin, ActorSurfaceOrigin::Agent)
    }

    pub const fn is_workflow(self) -> bool {
        matches!(self.origin, ActorSurfaceOrigin::Workflow)
    }

    pub const fn is_organization(self) -> bool {
        matches!(self.origin, ActorSurfaceOrigin::Organization)
    }
}

/// Invalid legacy metadata that claims more than one source-level actor origin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActorOriginConflict {
    pub is_workflow: bool,
    pub is_agent: bool,
    pub is_organization: bool,
}

impl std::fmt::Display for ActorOriginConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "actor metadata has conflicting origins (workflow={}, agent={}, organization={})",
            self.is_workflow, self.is_agent, self.is_organization
        )
    }
}

impl std::error::Error for ActorOriginConflict {}

/// Surface-level role carried by an actor after lowering.
///
/// `Agent`, `Workflow`, `Organization`, and `Virtual` are compatibility roles
/// describing how an actor was produced. They do not change the fact that the
/// executable runtime object is an actor.
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
    /// Return orthogonal semantic dimensions for this lowered actor.
    pub fn semantics(&self) -> Result<ActorSemantics, ActorOriginConflict> {
        ActorSemantics::from_legacy_flags(
            self.persistent,
            self.is_workflow,
            self.is_agent,
            self.is_organization,
            self.virtual_,
        )
    }

    /// Return the canonical semantic role of this lowered actor.
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
    /// Return orthogonal semantic dimensions encoded by compatibility metadata.
    pub fn semantics(&self) -> Result<ActorSemantics, ActorOriginConflict> {
        ActorSemantics::from_legacy_flags(
            self.persistent,
            self.is_workflow,
            self.is_agent,
            self.is_organization,
            self.is_virtual,
        )
    }

    /// Return the canonical role encoded in serialized actor metadata without
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
    /// Return orthogonal semantic dimensions for a live runtime actor.
    ///
    /// Live actors currently do not retain organization/virtual provenance, so
    /// those dimensions default to plain/eager until the serialized/runtime
    /// metadata migration is complete.
    pub fn semantics(&self) -> Result<ActorSemantics, ActorOriginConflict> {
        ActorSemantics::from_legacy_flags(
            self.persistent,
            self.is_workflow,
            self.is_agent,
            false,
            false,
        )
    }

    /// Return the canonical role of a live runtime actor.
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
    fn actor_semantics_keep_origin_activation_and_durability_orthogonal() {
        let semantics =
            ActorSemantics::from_legacy_flags(true, false, true, false, true).unwrap();

        assert_eq!(semantics.origin, ActorSurfaceOrigin::Agent);
        assert_eq!(semantics.activation, ActorActivation::Virtual);
        assert_eq!(semantics.durability, ActorDurability::Durable);
        assert!(semantics.is_agent());
    }

    #[test]
    fn actor_semantics_reject_conflicting_surface_origins() {
        assert!(ActorSemantics::from_legacy_flags(
            true, true, true, false, false
        )
        .is_err());
    }

    #[test]
    fn bytecode_and_runtime_expose_normalized_semantics() {
        let mut meta = crate::bytecode::ActorMeta::new("assistant");
        meta.persistent = true;
        meta.is_agent = true;
        meta.is_virtual = true;
        let semantics = meta.semantics().unwrap();
        assert_eq!(semantics.origin, ActorSurfaceOrigin::Agent);
        assert_eq!(semantics.activation, ActorActivation::Virtual);

        let mut actor = crate::runtime::Actor::new(1, "assistant", 0);
        actor.persistent = true;
        actor.is_agent = true;
        let semantics = actor.semantics().unwrap();
        assert_eq!(semantics.origin, ActorSurfaceOrigin::Agent);
        assert_eq!(semantics.durability, ActorDurability::Durable);
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
