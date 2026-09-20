//! Virtual actor (grain) registry and lifecycle support.
//!
//! Grains are Orleans-style virtual actors: they are addressed by a stable
//! `(grain_type, key)` identity, materialized on demand when a message is
//! sent to them, and dehydrated when idle.  This module holds the metadata
//! needed to construct a grain from its type and to hydrate it from a
//! persisted snapshot.

use super::persistence::StateModel;
use std::collections::HashMap;
use std::fmt;

/// Largest value representable by the current NaN-boxed ActorRef payload.
pub const MAX_ACTIVATION_HANDLE: u64 = 0x0000_FFFF_FFFF_FFFF;

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

    /// Unambiguous canonical encoding of the full logical identity.
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

    /// Stable 128-bit digest used for compact durable/directory identity.
    /// The full GrainId remains authoritative for collision resolution.
    pub fn logical_id(&self) -> LogicalActorId {
        LogicalActorId::from_grain(self)
    }
}

/// Stable 128-bit digest of a logical actor identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LogicalActorId([u8; 16]);

impl LogicalActorId {
    const DOMAIN: &'static [u8] = b"nulang.logical-actor.v1\\0";

    pub fn from_grain(grain: &GrainId) -> Self {
        let mut hasher = blake3::Hasher::new();
        hasher.update(Self::DOMAIN);
        hasher.update(&grain.canonical_bytes());
        let digest = hasher.finalize();
        let mut bytes = [0u8; 16];
        bytes.copy_from_slice(&digest.as_bytes()[..16]);
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }

    pub fn into_bytes(self) -> [u8; 16] {
        self.0
    }
}

impl fmt::Display for LogicalActorId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

/// Ephemeral runtime-local handle for one live activation.
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

/// Collision-free live mapping from full logical identities to compact handles.
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
            next_handle: ActivationHandle::MIN,
            by_grain: HashMap::new(),
            by_handle: HashMap::new(),
        }
    }

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

    pub fn logical_id_for(&self, handle: ActivationHandle) -> Option<LogicalActorId> {
        self.grain_for(handle).map(GrainId::logical_id)
    }

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

/// Legacy compatibility mapping from a grain identity to a 48-bit actor id.
///
/// This truncated hash is not a collision-free durable logical identity. New
/// identity-aware code should use GrainId/LogicalActorId and an activation
/// directory; this function remains byte-for-byte stable for existing runtime,
/// persistence, and tests during the migration window.
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
    fn logical_actor_id_is_stable_and_128_bit() {
        let a = GrainId::new("User", "user:42").logical_id();
        let b = GrainId::new("User", "user:42").logical_id();
        let c = GrainId::new("User", "user:43").logical_id();
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.as_bytes().len(), 16);
        assert_eq!(a.to_string().len(), 32);
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
        assert_eq!(directory.logical_id_for(ah), Some(a.logical_id()));
        assert_eq!(directory.len(), 2);
    }

    #[test]
    fn activation_handle_enforces_vm_payload_width() {
        assert_eq!(ActivationHandle::new(0), None);
        assert_eq!(ActivationHandle::new(MAX_ACTIVATION_HANDLE + 1), None);
        assert_eq!(ActivationHandle::new(1).unwrap().get(), 1);
        assert_eq!(
            ActivationHandle::new(MAX_ACTIVATION_HANDLE).unwrap().get(),
            MAX_ACTIVATION_HANDLE
        );
    }

    #[test]
    fn legacy_grain_actor_id_fixture_is_stable() {
        let g = GrainId::new("User", "user:42");
        assert_eq!(grain_actor_id(&g), 0x10B0_DD6B_B828);
    }

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
