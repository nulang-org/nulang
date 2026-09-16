//! Virtual actor (grain) registry and lifecycle support.
//!
//! Grains are Orleans-style virtual actors: they are addressed by a stable
//! `(grain_type, key)` identity, materialized on demand when a message is
//! sent to them, and dehydrated when idle.  This module holds the metadata
//! needed to construct a grain from its type and to hydrate it from a
//! persisted snapshot.

use super::persistence::StateModel;
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
}
