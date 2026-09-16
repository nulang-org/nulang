//! Virtual actor (grain) registry and lifecycle support.
//!
//! Grains are Orleans-style virtual actors: they are addressed by a stable
//! `(grain_type, key)` identity, materialized on demand when a message is
//! sent to them, and dehydrated when idle.  This module holds the metadata
//! needed to construct a grain from its type and to hydrate it from a
//! persisted snapshot.

use super::persistence::StateModel;
use crate::types::{NuError, Span};
use std::collections::HashMap;

/// Stable identity of a virtual actor.
///
/// `GrainId` is the logical identity. The `u64` returned by
/// [`grain_actor_id`] is only the current compact runtime/persistence
/// projection used by `Value::actor_ref`; it must not be treated as a
/// collision-free replacement for `(grain_type, key)`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GrainId {
    pub grain_type: String,
    pub key: String,
}

impl GrainId {
    /// Create a new grain id.
    pub fn new(grain_type: impl Into<String>, key: impl Into<String>) -> Self {
        GrainId {
            grain_type: grain_type.into(),
            key: key.into(),
        }
    }

    /// Render as a human-readable name for the actor.
    ///
    /// This is deliberately not a canonical serialization. Identity-sensitive
    /// code must continue to use the structured `(grain_type, key)` pair.
    pub fn actor_name(&self) -> String {
        format!("{}@{}", self.grain_type, self.key)
    }
}

/// Error returned when two distinct logical grains attempt to claim the same
/// compact actor id.
///
/// The 48-bit actor-ref projection is intentionally compact, not unique. This
/// error is therefore part of the runtime's correctness boundary: callers must
/// fail closed rather than overwriting the existing reverse mapping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrainActorIdCollision {
    pub actor_id: u64,
    pub existing: GrainId,
    pub attempted: GrainId,
}

impl std::fmt::Display for GrainActorIdCollision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "grain actor-id collision at {}: existing {:?}, attempted {:?}",
            self.actor_id, self.existing, self.attempted
        )
    }
}

impl std::error::Error for GrainActorIdCollision {}

impl From<GrainActorIdCollision> for NuError {
    fn from(error: GrainActorIdCollision) -> Self {
        NuError::RuntimeError {
            msg: error.to_string(),
            span: Span::new(0, 0),
        }
    }
}

/// Failure to establish the complete set of indexes for a resident grain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrainIdentityBindingError {
    /// A compact actor id is already owned by another logical grain.
    CompactIdCollision(GrainActorIdCollision),
    /// The same logical grain is already resident under another actor id.
    LogicalIdentityConflict {
        grain_id: GrainId,
        existing_actor_id: u64,
        attempted_actor_id: u64,
    },
}

impl std::fmt::Display for GrainIdentityBindingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CompactIdCollision(error) => write!(f, "{error}"),
            Self::LogicalIdentityConflict {
                grain_id,
                existing_actor_id,
                attempted_actor_id,
            } => write!(
                f,
                "grain logical-identity conflict for {:?}: existing actor {}, attempted actor {}",
                grain_id, existing_actor_id, attempted_actor_id
            ),
        }
    }
}

impl std::error::Error for GrainIdentityBindingError {}

impl From<GrainActorIdCollision> for GrainIdentityBindingError {
    fn from(error: GrainActorIdCollision) -> Self {
        Self::CompactIdCollision(error)
    }
}

impl From<GrainIdentityBindingError> for NuError {
    fn from(error: GrainIdentityBindingError) -> Self {
        NuError::RuntimeError {
            msg: error.to_string(),
            span: Span::new(0, 0),
        }
    }
}

fn validate_grain_actor_id_binding(
    bindings: &HashMap<u64, GrainId>,
    actor_id: u64,
    grain_id: &GrainId,
) -> Result<(), GrainActorIdCollision> {
    match bindings.get(&actor_id) {
        None => Ok(()),
        Some(existing) if existing == grain_id => Ok(()),
        Some(existing) => Err(GrainActorIdCollision {
            actor_id,
            existing: existing.clone(),
            attempted: grain_id.clone(),
        }),
    }
}

/// Establish an `actor_id -> GrainId` reverse binding without permitting a
/// distinct logical identity to overwrite an existing one.
///
/// This is the only safe primitive for populating a single compact grain-id
/// reverse index. It is deliberately idempotent for an identical binding so
/// repeated hydration/registration does not fail.
pub fn bind_grain_actor_id(
    bindings: &mut HashMap<u64, GrainId>,
    actor_id: u64,
    grain_id: GrainId,
) -> Result<(), GrainActorIdCollision> {
    validate_grain_actor_id_binding(bindings, actor_id, &grain_id)?;
    bindings.entry(actor_id).or_insert(grain_id);
    Ok(())
}

/// Validate the complete set of grain identity indexes without mutating them.
///
/// Runtime activation should call this *before* recovery-module registration,
/// snapshot hydration, persistence access with side effects, or actor insertion.
/// After activation succeeds, [`bind_resident_grain_indexes`] commits the exact
/// same validated relation. This two-phase protocol prevents both collision
/// aliasing and phantom resident mappings after a failed hydration.
pub fn validate_resident_grain_indexes(
    grain_residents: &HashMap<GrainId, u64>,
    actor_grain_id: &HashMap<u64, GrainId>,
    grain_actor_ids: &HashMap<u64, GrainId>,
    actor_id: u64,
    grain_id: &GrainId,
) -> Result<(), GrainIdentityBindingError> {
    validate_grain_actor_id_binding(actor_grain_id, actor_id, grain_id)?;
    validate_grain_actor_id_binding(grain_actor_ids, actor_id, grain_id)?;

    if let Some(&existing_actor_id) = grain_residents.get(grain_id) {
        if existing_actor_id != actor_id {
            return Err(GrainIdentityBindingError::LogicalIdentityConflict {
                grain_id: grain_id.clone(),
                existing_actor_id,
                attempted_actor_id: actor_id,
            });
        }
    }

    Ok(())
}

/// Atomically validate and establish the three indexes used by a resident
/// grain activation.
///
/// Validation is completed against every index before any map is mutated. A
/// collision or logical rebinding therefore cannot leave the runtime with a
/// partially-updated set of indexes. For activation paths that can fail after
/// identity validation, call [`validate_resident_grain_indexes`] first, perform
/// activation, and call this function only after activation succeeds.
pub fn bind_resident_grain_indexes(
    grain_residents: &mut HashMap<GrainId, u64>,
    actor_grain_id: &mut HashMap<u64, GrainId>,
    grain_actor_ids: &mut HashMap<u64, GrainId>,
    actor_id: u64,
    grain_id: GrainId,
) -> Result<(), GrainIdentityBindingError> {
    validate_resident_grain_indexes(
        grain_residents,
        actor_grain_id,
        grain_actor_ids,
        actor_id,
        &grain_id,
    )?;

    grain_residents.entry(grain_id.clone()).or_insert(actor_id);
    actor_grain_id
        .entry(actor_id)
        .or_insert_with(|| grain_id.clone());
    grain_actor_ids.entry(actor_id).or_insert(grain_id);
    Ok(())
}

/// Policy controlling when a grain may be dehydrated / evicted.
#[derive(Debug, Clone, Copy)]
pub struct DehydratePolicy {
    /// Idle milliseconds before the runtime may hibernate the grain.
    pub idle_ms: u64,
    /// Whether the grain may be dehydrated at all.
    pub allow_dehydrate: bool,
}

impl Default for DehydratePolicy {
    fn default() -> Self {
        DehydratePolicy {
            idle_ms: 30_000,
            allow_dehydrate: true,
        }
    }
}

/// Metadata for a single grain type, used to hydrate instances.
#[derive(Debug, Clone)]
pub struct GrainType {
    /// Module containing the bytecode behavior table for this grain.
    pub module: crate::bytecode::CodeModule,
    /// Default state models parsed from the `entity` declaration.
    pub default_models: Vec<(String, StateModel)>,
    /// Bytecode offsets parallel to the module behavior table.
    pub bytecode_offsets: Vec<usize>,
    /// Compensation offsets (for saga workflows) parallel to behavior table.
    pub compensation_offsets: Vec<Option<usize>>,
    /// Dehydration / eviction policy for this grain type.
    pub dehydrate_policy: DehydratePolicy,
}

/// Registry of all virtual actor types known to a runtime.
#[derive(Debug, Default)]
pub struct GrainRegistry {
    types: HashMap<String, GrainType>,
}

impl GrainRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        GrainRegistry {
            types: HashMap::new(),
        }
    }

    /// Register a grain type.
    pub fn register(&mut self, name: impl Into<String>, grain_type: GrainType) {
        self.types.insert(name.into(), grain_type);
    }

    /// Look up a grain type by name.
    pub fn get(&self, name: &str) -> Option<&GrainType> {
        self.types.get(name)
    }

    /// Look up a grain type by name mutably.
    pub fn get_mut(&mut self, name: &str) -> Option<&mut GrainType> {
        self.types.get_mut(name)
    }

    /// True if the registry contains the named grain type.
    pub fn contains(&self, name: &str) -> bool {
        self.types.contains_key(name)
    }

    /// Iterate over registered grain types.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &GrainType)> {
        self.types.iter()
    }
}

/// Version of the persisted grain-id projection below.
///
/// Version 1 is already present in snapshots, journals, actor references, and
/// shard routing. Never change its hash construction in place. A future
/// projection must use a new version and an explicit persisted-state migration.
pub const GRAIN_ACTOR_ID_ALGORITHM_VERSION: u8 = 1;

/// Number of payload bits available in `Value::actor_ref`.
pub const GRAIN_ACTOR_ID_BITS: u32 = 48;
const GRAIN_ACTOR_ID_MASK: u64 = (1_u64 << GRAIN_ACTOR_ID_BITS) - 1;

/// Deterministically project a logical grain identity to the v1 compact actor
/// id used by the runtime.
///
/// # Stability contract
///
/// This mapping is persistence ABI. The exact FNV-1a construction, separator,
/// and 48-bit mask are frozen for version 1. Changing any of them would make
/// existing durable grains appear under different actor ids after an upgrade.
/// Add a v2 mapping plus migration instead.
///
/// # Collision contract
///
/// The result is only 48 bits, so this projection is *not collision-free*.
/// `GrainId` remains the authoritative logical identity. Runtime code that
/// establishes an `actor_id -> GrainId` binding must reject a second, different
/// `GrainId` that projects to an already-bound actor id rather than silently
/// aliasing the two grains.
pub fn grain_actor_id(grain: &GrainId) -> u64 {
    let mut hash: u64 = 0xCBF29CE484222325; // FNV-1a offset basis
    const PRIME: u64 = 0x00000100000001B3;

    for b in grain.grain_type.as_bytes() {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(PRIME);
    }

    // 0xFF is not a valid UTF-8 byte, so this delimiter cannot occur inside
    // either Rust String's UTF-8 representation. It therefore keeps
    // `(grain_type, key)` pairs unambiguous within the v1 preimage format.
    hash ^= 0xFF;
    hash = hash.wrapping_mul(PRIME);

    for b in grain.key.as_bytes() {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(PRIME);
    }

    hash & GRAIN_ACTOR_ID_MASK
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_grain_actor_id_deterministic() {
        let g = GrainId::new("User", "user:42");
        let id1 = grain_actor_id(&g);
        let id2 = grain_actor_id(&g);
        assert_eq!(id1, id2);
        assert!(id1 < (1_u64 << GRAIN_ACTOR_ID_BITS));
    }

    #[test]
    fn test_grain_actor_id_distinct_keys() {
        let a = grain_actor_id(&GrainId::new("User", "a"));
        let b = grain_actor_id(&GrainId::new("User", "b"));
        assert_ne!(a, b);
    }

    #[test]
    fn test_grain_actor_id_v1_golden_vectors() {
        // These values freeze the persistence ABI. If this test needs to
        // change, the implementation needs a versioned identity migration,
        // not merely updated expectations.
        let vectors = [
            (("User", "user:42"), 0x10b0_dd6b_b828_u64),
            (("Counter", "alpha"), 0xf7db_4b82_eda8_u64),
            (("User", "42"), 0x55df_c955_ef51_u64),
            (("", ""), 0x724c_8602_eb6e_u64),
            (("é", "🔑"), 0xc77d_1e7d_77bc_u64),
        ];

        for ((grain_type, key), expected) in vectors {
            assert_eq!(
                grain_actor_id(&GrainId::new(grain_type, key)),
                expected,
                "grain actor-id v1 mapping changed for ({grain_type:?}, {key:?})"
            );
        }
    }

    #[test]
    fn test_actor_name_is_not_identity_serialization() {
        // Human-readable names can be ambiguous; structured GrainId equality
        // must remain the source of truth for logical identity.
        let a = GrainId::new("a@b", "c");
        let b = GrainId::new("a", "b@c");
        assert_eq!(a.actor_name(), b.actor_name());
        assert_ne!(a, b);
        assert_ne!(grain_actor_id(&a), grain_actor_id(&b));
    }

    #[test]
    fn test_bind_grain_actor_id_allows_first_binding() {
        let mut bindings = HashMap::new();
        let grain = GrainId::new("User", "42");

        assert_eq!(bind_grain_actor_id(&mut bindings, 7, grain.clone()), Ok(()));
        assert_eq!(bindings.get(&7), Some(&grain));
    }

    #[test]
    fn test_bind_grain_actor_id_is_idempotent_for_same_identity() {
        let mut bindings = HashMap::new();
        let grain = GrainId::new("User", "42");
        bind_grain_actor_id(&mut bindings, 7, grain.clone()).unwrap();

        assert_eq!(bind_grain_actor_id(&mut bindings, 7, grain.clone()), Ok(()));
        assert_eq!(bindings.len(), 1);
        assert_eq!(bindings.get(&7), Some(&grain));
    }

    #[test]
    fn test_bind_grain_actor_id_rejects_collision_without_overwrite() {
        let mut bindings = HashMap::new();
        let original = GrainId::new("User", "42");
        let colliding = GrainId::new("Order", "42");
        bind_grain_actor_id(&mut bindings, 7, original.clone()).unwrap();

        let err = bind_grain_actor_id(&mut bindings, 7, colliding.clone()).unwrap_err();
        assert_eq!(
            err,
            GrainActorIdCollision {
                actor_id: 7,
                existing: original.clone(),
                attempted: colliding,
            }
        );
        assert_eq!(bindings.len(), 1);
        assert_eq!(bindings.get(&7), Some(&original));
    }

    #[test]
    fn test_validate_resident_grain_indexes_is_pure() {
        let original = GrainId::new("User", "42");
        let attempted = GrainId::new("Order", "42");
        let residents = HashMap::new();
        let actor_to_grain = HashMap::new();
        let mut known_ids = HashMap::new();
        known_ids.insert(7, original.clone());

        let error = validate_resident_grain_indexes(
            &residents,
            &actor_to_grain,
            &known_ids,
            7,
            &attempted,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            GrainIdentityBindingError::CompactIdCollision(_)
        ));
        assert!(residents.is_empty());
        assert!(actor_to_grain.is_empty());
        assert_eq!(known_ids.get(&7), Some(&original));
    }

    #[test]
    fn test_resident_grain_binding_updates_all_indexes() {
        let mut residents = HashMap::new();
        let mut actor_to_grain = HashMap::new();
        let mut known_ids = HashMap::new();
        let grain = GrainId::new("User", "42");

        bind_resident_grain_indexes(
            &mut residents,
            &mut actor_to_grain,
            &mut known_ids,
            7,
            grain.clone(),
        )
        .unwrap();

        assert_eq!(residents.get(&grain), Some(&7));
        assert_eq!(actor_to_grain.get(&7), Some(&grain));
        assert_eq!(known_ids.get(&7), Some(&grain));
    }

    #[test]
    fn test_resident_grain_binding_is_idempotent() {
        let mut residents = HashMap::new();
        let mut actor_to_grain = HashMap::new();
        let mut known_ids = HashMap::new();
        let grain = GrainId::new("User", "42");

        bind_resident_grain_indexes(
            &mut residents,
            &mut actor_to_grain,
            &mut known_ids,
            7,
            grain.clone(),
        )
        .unwrap();
        bind_resident_grain_indexes(
            &mut residents,
            &mut actor_to_grain,
            &mut known_ids,
            7,
            grain.clone(),
        )
        .unwrap();

        assert_eq!(residents.len(), 1);
        assert_eq!(actor_to_grain.len(), 1);
        assert_eq!(known_ids.len(), 1);
    }

    #[test]
    fn test_resident_grain_binding_collision_is_atomic() {
        let original = GrainId::new("User", "42");
        let attempted = GrainId::new("Order", "42");
        let mut residents = HashMap::new();
        let mut actor_to_grain = HashMap::new();
        let mut known_ids = HashMap::new();
        known_ids.insert(7, original.clone());

        let error = bind_resident_grain_indexes(
            &mut residents,
            &mut actor_to_grain,
            &mut known_ids,
            7,
            attempted.clone(),
        )
        .unwrap_err();

        assert_eq!(
            error,
            GrainIdentityBindingError::CompactIdCollision(GrainActorIdCollision {
                actor_id: 7,
                existing: original.clone(),
                attempted,
            })
        );
        assert!(residents.is_empty());
        assert!(actor_to_grain.is_empty());
        assert_eq!(known_ids.get(&7), Some(&original));
    }

    #[test]
    fn test_resident_grain_logical_rebind_is_atomic() {
        let grain = GrainId::new("User", "42");
        let mut residents = HashMap::new();
        let mut actor_to_grain = HashMap::new();
        let mut known_ids = HashMap::new();
        residents.insert(grain.clone(), 7);

        let error = bind_resident_grain_indexes(
            &mut residents,
            &mut actor_to_grain,
            &mut known_ids,
            8,
            grain.clone(),
        )
        .unwrap_err();

        assert_eq!(
            error,
            GrainIdentityBindingError::LogicalIdentityConflict {
                grain_id: grain.clone(),
                existing_actor_id: 7,
                attempted_actor_id: 8,
            }
        );
        assert_eq!(residents.get(&grain), Some(&7));
        assert!(actor_to_grain.is_empty());
        assert!(known_ids.is_empty());
    }

    #[test]
    fn test_grain_actor_id_collision_converts_to_runtime_error() {
        let error = GrainActorIdCollision {
            actor_id: 7,
            existing: GrainId::new("User", "42"),
            attempted: GrainId::new("Order", "42"),
        };
        let runtime_error: NuError = error.into();
        assert!(runtime_error
            .to_string()
            .contains("grain actor-id collision at 7"));
    }

    #[test]
    fn test_grain_identity_binding_error_converts_to_runtime_error() {
        let error = GrainIdentityBindingError::LogicalIdentityConflict {
            grain_id: GrainId::new("User", "42"),
            existing_actor_id: 7,
            attempted_actor_id: 8,
        };
        let runtime_error: NuError = error.into();
        assert!(runtime_error
            .to_string()
            .contains("grain logical-identity conflict"));
    }
}
