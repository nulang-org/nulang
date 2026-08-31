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

/// Deterministically map a grain identity to a stable actor id.
///
/// Actor references store their payload in 48 bits (`Value::actor_ref`),
/// so the returned id is masked to 48 bits.  This keeps grain identities
/// addressable as ordinary actor refs while leaving the NaN tag bits
/// untouched.
pub fn grain_actor_id(grain: &GrainId) -> u64 {
    let mut hash: u64 = 0xCBF29CE484222325; // FNV offset basis
    const PRIME: u64 = 0x00000100000001B3;

    for b in grain.grain_type.as_bytes() {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(PRIME);
    }
    // Separator byte unlikely in identifiers.
    hash ^= 0xFF;
    hash = hash.wrapping_mul(PRIME);
    for b in grain.key.as_bytes() {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(PRIME);
    }

    hash & 0x0000_FFFF_FFFF_FFFF
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
        assert_eq!(id1 & 0x8000_0000_0000_0000, 0);
    }

    #[test]
    fn test_grain_actor_id_distinct_keys() {
        let a = grain_actor_id(&GrainId::new("User", "a"));
        let b = grain_actor_id(&GrainId::new("User", "b"));
        assert_ne!(a, b);
    }
}
