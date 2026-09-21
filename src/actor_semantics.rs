//! Runtime-neutral actor semantic descriptors.
//!
//! This module deliberately contains no VM, scheduler, persistence, networking,
//! or native-runtime dependencies. The compiler, browser playground, bytecode
//! layer, and native runtime can therefore share one definition of actor
//! semantics without importing runtime implementation details.

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
/// actor role. Keeping it separate allows combinations such as virtual agents
/// or workflows without introducing another runtime species.
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
/// the serialized format can encode these dimensions directly.
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

    pub const fn is_virtual(self) -> bool {
        matches!(self.activation, ActorActivation::Virtual)
    }

    pub const fn is_durable(self) -> bool {
        matches!(self.durability, ActorDurability::Durable)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dimensions_are_orthogonal() {
        let semantics =
            ActorSemantics::from_legacy_flags(true, false, true, false, true).unwrap();

        assert_eq!(semantics.origin, ActorSurfaceOrigin::Agent);
        assert_eq!(semantics.activation, ActorActivation::Virtual);
        assert_eq!(semantics.durability, ActorDurability::Durable);
        assert!(semantics.is_agent());
        assert!(semantics.is_virtual());
        assert!(semantics.is_durable());
    }

    #[test]
    fn conflicting_origins_are_rejected() {
        assert!(ActorSemantics::from_legacy_flags(true, true, true, false, false).is_err());
    }
}
