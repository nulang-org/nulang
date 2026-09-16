//! Virtual actor (grain) registry and lifecycle support.
//!
//! Grains are Orleans-style virtual actors: they are addressed by a stable
//! `(grain_type, key)` identity, materialized on demand when a message is
//! sent to them, and dehydrated when idle. This module holds the metadata
//! needed to construct a grain from its type and to hydrate it from a
//! persisted snapshot.
//!
//! # Identity model
//!
//! A [`GrainId`] is the durable logical identity of a virtual actor. It must
//! never be replaced by, or reconstructed from, a truncated runtime handle.
//! [`ActivationHandle`] is deliberately a separate, runtime-local identifier
//! that fits in the current NaN-boxed `ActorRef` payload. [`ActivationDirectory`]
//! owns the bijection between the two for a runtime instance.
//!
//! `grain_actor_id` remains as a compatibility bridge for the current runtime
//! and persistence format. New code must not treat its 48-bit hash as a
//! collision-free durable identity; migration work should use `GrainId` as the
//! persistence/directory key and an `ActivationHandle` only for live routing.

use super::persistence::StateModel;
use std::collections::HashMap;
use std::fmt;

/// Largest value representable by the current NaN-boxed ActorRef payload.
pub const MAX_ACTIVATION_HANDLE: u64 = 0x0000_FFFF_FFFF_FFFF;

/// Stable logical identity of a virtual actor.
///
/// Equality is defined by the complete `(grain_type, key)` pair. The runtime
/// must retain this full identity instead of relying on a truncated hash.
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

    /// Return an unambiguous canonical byte representation suitable for
    /// hashing, signatures, directory keys, or future persistence adapters.
    ///
    /// Length-prefixing avoids separator ambiguities such as `(ab, c)` versus
    /// `(a, bc)` without constraining the strings themselves.
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let type_bytes = self.grain_type.as_bytes();
        let key_bytes = self.key.as_bytes();
        let mut out = Vec::with_capacity(8 + type_bytes.len() + key_bytes.len());
        out.extend_from_slice(&(type_bytes.len() as u32).to_be_bytes());
        out.extend_from_slice(type_bytes);
        out.extend_from_slice(&(key_bytes.len() as u32).to_be_bytes());
        out.extend_from_slice(key_bytes);
        out
    }
}

/// Ephemeral runtime handle for one currently addressable activation.
///
/// Handles intentionally fit inside the existing 48-bit NaN-box payload. They
/// are not durable entity IDs and must not be persisted as the sole identity
/// of a virtual actor once the identity migration is complete.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ActivationHandle(u64);

impl ActivationHandle {
    pub const MIN: u64 = 1;
    pub const MAX: u64 = MAX_ACTIVATION_HANDLE;

    pub fn new(raw: u64) -> Option<Self> {
        if (Self::MIN..=Self::MAX).contains(&raw) {
            Some(Self(raw))
        } else {
            None
        }
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

/// Allocation failure for the live activation directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivationDirectoryError {
    Exhausted,
}

impl fmt::Display for ActivationDirectoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ActivationDirectoryError::Exhausted => {
                write!(f, "virtual actor activation-handle space exhausted")
            }
        }
    }
}

impl std::error::Error for ActivationDirectoryError {}

/// Runtime-local bijection between stable logical grain identities and compact
/// activation handles.
///
/// This removes hash collisions from the live addressing layer: two distinct
/// `GrainId` values can never resolve to the same handle within one directory.
/// Handles are intentionally ephemeral; a future distributed grain directory
/// may assign a different activation after restart or migration while keeping
/// the same `GrainId`.
#[derive(Debug)]
pub struct ActivationDirectory {
    next_handle: u64,
    by_grain: HashMap<GrainId, ActivationHandle>,
    by_handle: HashMap<ActivationHandle, GrainId>,
}

impl Default for ActivationDirectory {
    fn default() -> Self {
        Self::new()
    }
}

impl ActivationDirectory {
    pub fn new() -> Self {
        Self {
            // Reserve zero as the invalid/null actor reference.
            next_handle: ActivationHandle::MIN,
            by_grain: HashMap::new(),
            by_handle: HashMap::new(),
        }
    }

    /// Resolve an existing activation or allocate a new collision-free handle.
    pub fn resolve_or_allocate(
        &mut self,
        grain_id: GrainId,
    ) -> Result<ActivationHandle, ActivationDirectoryError> {
        if let Some(handle) = self.by_grain.get(&grain_id).copied() {
            return Ok(handle);
        }

        let raw = self.next_handle;
        let handle = ActivationHandle::new(raw).ok_or(ActivationDirectoryError::Exhausted)?;
        self.next_handle = raw
            .checked_add(1)
            .ok_or(ActivationDirectoryError::Exhausted)?;

        self.by_grain.insert(grain_id.clone(), handle);
        self.by_handle.insert(handle, grain_id);
        Ok(handle)
    }

    pub fn handle_for(&self, grain_id: &GrainId) -> Option<ActivationHandle> {
        self.by_grain.get(grain_id).copied()
    }

    pub fn grain_for(&self, handle: ActivationHandle) -> Option<&GrainId> {
        self.by_handle.get(&handle)
    }

    /// Remove the live activation mapping. This does not delete durable entity
    /// state; the logical `GrainId` remains the identity used to hydrate later.
    pub fn remove(&mut self, grain_id: &GrainId) -> Option<ActivationHandle> {
        let handle = self.by_grain.remove(grain_id)?;
        self.by_handle.remove(&handle);
        Some(handle)
    }

    pub fn len(&self) -> usize {
        self.by_grain.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_grain.is_empty()
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

/// Legacy deterministic mapping from a grain identity to a 48-bit actor id.
///
/// # Compatibility only
///
/// The 48-bit result is collision-prone at sufficiently large populations and
/// therefore must not be treated as the durable logical identity of a virtual
/// actor. It is retained while the runtime/persistence wire formats migrate to
/// `GrainId` + `ActivationDirectory` semantics.
pub fn grain_actor_id(grain: &GrainId) -> u64 {
    let mut hash: u64 = 0xCBF29CE484222325; // FNV offset basis
    const PRIME: u64 = 0x00000100000001B3;

    for b in grain.canonical_bytes() {
        hash ^= b as u64;
        hash = hash.wrapping_mul(PRIME);
    }

    hash & MAX_ACTIVATION_HANDLE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_identity_is_unambiguous() {
        let a = GrainId::new("ab", "c");
        let b = GrainId::new("a", "bc");
        assert_ne!(a.canonical_bytes(), b.canonical_bytes());
    }

    #[test]
    fn activation_handle_rejects_invalid_values() {
        assert_eq!(ActivationHandle::new(0), None);
        assert_eq!(ActivationHandle::new(MAX_ACTIVATION_HANDLE + 1), None);
        assert_eq!(ActivationHandle::new(1).unwrap().get(), 1);
        assert_eq!(
            ActivationHandle::new(MAX_ACTIVATION_HANDLE).unwrap().get(),
            MAX_ACTIVATION_HANDLE
        );
    }

    #[test]
    fn activation_directory_is_bijective() {
        let mut directory = ActivationDirectory::new();
        let a = GrainId::new("User", "a");
        let b = GrainId::new("User", "b");

        let ah = directory.resolve_or_allocate(a.clone()).unwrap();
        let bh = directory.resolve_or_allocate(b.clone()).unwrap();

        assert_ne!(ah, bh);
        assert_eq!(directory.resolve_or_allocate(a.clone()).unwrap(), ah);
        assert_eq!(directory.handle_for(&a), Some(ah));
        assert_eq!(directory.grain_for(ah), Some(&a));
        assert_eq!(directory.grain_for(bh), Some(&b));
        assert_eq!(directory.len(), 2);
    }

    #[test]
    fn activation_directory_removal_preserves_logical_identity_value() {
        let mut directory = ActivationDirectory::new();
        let grain = GrainId::new("Cart", "customer-42");
        let handle = directory.resolve_or_allocate(grain.clone()).unwrap();

        assert_eq!(directory.remove(&grain), Some(handle));
        assert_eq!(directory.handle_for(&grain), None);
        assert_eq!(directory.grain_for(handle), None);
        assert!(directory.is_empty());
        assert_eq!(grain, GrainId::new("Cart", "customer-42"));
    }

    #[test]
    fn test_grain_actor_id_deterministic_legacy_bridge() {
        let g = GrainId::new("User", "user:42");
        let id1 = grain_actor_id(&g);
        let id2 = grain_actor_id(&g);
        assert_eq!(id1, id2);
        assert!(id1 <= MAX_ACTIVATION_HANDLE);
    }

    #[test]
    fn test_grain_actor_id_distinct_keys_smoke_test() {
        let a = grain_actor_id(&GrainId::new("User", "a"));
        let b = grain_actor_id(&GrainId::new("User", "b"));
        assert_ne!(a, b);
    }
}
