//! Virtual actor (grain) registry and lifecycle support.
//!
//! Grains are Orleans-style virtual actors: they are addressed by a stable
//! `(grain_type, key)` identity, materialized on demand when a message is
//! sent to them, and dehydrated when idle.  This module holds the metadata
//! needed to construct a grain from its type and to hydrate it from a
//! persisted snapshot.

use super::persistence::StateModel;
use std::collections::{HashMap, HashSet};
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
        let mut out = Vec::with_capacity(16 + type_bytes.len() + key_bytes.len());
        out.extend_from_slice(&(type_bytes.len() as u64).to_be_bytes());
        out.extend_from_slice(type_bytes);
        out.extend_from_slice(&(key_bytes.len() as u64).to_be_bytes());
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
    const DOMAIN: &'static [u8] = b"nulang.logical-actor.v1\0";

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

/// Monotonic generation for successive live activations of one logical actor.
///
/// Epoch zero is intentionally invalid so an absent/default value cannot be
/// mistaken for an authoritative activation. The directory retains the last
/// epoch after deactivation so a later activation is always strictly newer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ActivationEpoch(u64);

impl ActivationEpoch {
    pub const INITIAL: Self = Self(1);

    pub fn new(raw: u64) -> Option<Self> {
        (raw >= Self::INITIAL.0).then_some(Self(raw))
    }

    pub fn get(self) -> u64 {
        self.0
    }

    fn next(self) -> Option<Self> {
        self.0.checked_add(1).and_then(Self::new)
    }
}

/// Runtime-local identity of one specific activation incarnation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ActivationStamp {
    pub handle: ActivationHandle,
    pub epoch: ActivationEpoch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivationDirectoryError {
    Exhausted,
    EpochExhausted,
    AuthorityRequired,
    StaleAuthority,
}

impl fmt::Display for ActivationDirectoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ActivationDirectoryError::Exhausted => {
                write!(f, "virtual actor activation-handle space exhausted")
            }
            ActivationDirectoryError::EpochExhausted => {
                write!(f, "virtual actor activation epoch space exhausted")
            }
            ActivationDirectoryError::AuthorityRequired => {
                write!(f, "virtual actor requires an externally granted activation epoch")
            }
            ActivationDirectoryError::StaleAuthority => {
                write!(f, "virtual actor activation authority is stale")
            }
        }
    }
}

impl std::error::Error for ActivationDirectoryError {}

/// Collision-free live mapping from full logical identities to compact handles.
#[derive(Debug)]
pub struct ActivationDirectory {
    next_handle: u64,
    by_grain: HashMap<GrainId, ActivationStamp>,
    by_handle: HashMap<ActivationHandle, GrainId>,
    last_epoch: HashMap<GrainId, ActivationEpoch>,
    externally_fenced: HashSet<GrainId>,
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
            last_epoch: HashMap::new(),
            externally_fenced: HashSet::new(),
        }
    }

    pub fn resolve_or_allocate(
        &mut self,
        grain_id: GrainId,
    ) -> Result<ActivationHandle, ActivationDirectoryError> {
        self.resolve_or_activate(grain_id).map(|stamp| stamp.handle)
    }

    /// Resolve the current activation or allocate a strictly newer incarnation.
    pub fn resolve_or_activate(
        &mut self,
        grain_id: GrainId,
    ) -> Result<ActivationStamp, ActivationDirectoryError> {
        if let Some(stamp) = self.by_grain.get(&grain_id).copied() {
            return Ok(stamp);
        }
        if self.externally_fenced.contains(&grain_id) {
            return Err(ActivationDirectoryError::AuthorityRequired);
        }

        let raw = self.next_handle;
        let handle = ActivationHandle::new(raw).ok_or(ActivationDirectoryError::Exhausted)?;
        self.next_handle = raw
            .checked_add(1)
            .ok_or(ActivationDirectoryError::Exhausted)?;

        let epoch = match self.last_epoch.get(&grain_id).copied() {
            Some(previous) => previous
                .next()
                .ok_or(ActivationDirectoryError::EpochExhausted)?,
            None => ActivationEpoch::INITIAL,
        };
        let stamp = ActivationStamp { handle, epoch };

        self.last_epoch.insert(grain_id.clone(), epoch);
        self.by_grain.insert(grain_id.clone(), stamp);
        self.by_handle.insert(handle, grain_id);
        Ok(stamp)
    }

    /// Observe an epoch established by an external/distributed authority.
    ///
    /// A strictly newer observation invalidates any older live local activation
    /// and prevents autonomous reactivation. The caller must later install an
    /// explicit ownership grant with `install_authoritative_activation`.
    ///
    /// Returns the local activation stamp that was fenced, if any.
    pub fn observe_authoritative_epoch(
        &mut self,
        grain_id: GrainId,
        epoch: ActivationEpoch,
    ) -> Option<ActivationStamp> {
        if self
            .last_epoch
            .get(&grain_id)
            .is_some_and(|known| *known >= epoch)
        {
            return None;
        }

        self.last_epoch.insert(grain_id.clone(), epoch);
        self.externally_fenced.insert(grain_id.clone());

        let stale = self
            .by_grain
            .get(&grain_id)
            .copied()
            .filter(|stamp| stamp.epoch < epoch);
        if let Some(stamp) = stale {
            self.by_grain.remove(&grain_id);
            self.by_handle.remove(&stamp.handle);
        }
        stale
    }

    /// Install an activation epoch granted by the distributed ownership layer.
    ///
    /// This is the only path that clears an external fence. It trusts the caller
    /// to have established node ownership (lease/quorum/consensus policy lives
    /// above this local directory) and rejects grants older than the highest
    /// epoch already observed for the full logical identity.
    pub fn install_authoritative_activation(
        &mut self,
        grain_id: GrainId,
        epoch: ActivationEpoch,
    ) -> Result<ActivationStamp, ActivationDirectoryError> {
        if let Some(current) = self.by_grain.get(&grain_id).copied() {
            return if current.epoch == epoch {
                Ok(current)
            } else {
                Err(ActivationDirectoryError::StaleAuthority)
            };
        }
        if self
            .last_epoch
            .get(&grain_id)
            .is_some_and(|known| *known > epoch)
        {
            return Err(ActivationDirectoryError::StaleAuthority);
        }

        let raw = self.next_handle;
        let handle = ActivationHandle::new(raw).ok_or(ActivationDirectoryError::Exhausted)?;
        self.next_handle = raw
            .checked_add(1)
            .ok_or(ActivationDirectoryError::Exhausted)?;
        let stamp = ActivationStamp { handle, epoch };

        self.last_epoch.insert(grain_id.clone(), epoch);
        self.externally_fenced.remove(&grain_id);
        self.by_grain.insert(grain_id.clone(), stamp);
        self.by_handle.insert(handle, grain_id);
        Ok(stamp)
    }

    pub fn handle_for(&self, grain_id: &GrainId) -> Option<ActivationHandle> {
        self.by_grain.get(grain_id).map(|stamp| stamp.handle)
    }

    pub fn stamp_for(&self, grain_id: &GrainId) -> Option<ActivationStamp> {
        self.by_grain.get(grain_id).copied()
    }

    pub fn epoch_for(&self, grain_id: &GrainId) -> Option<ActivationEpoch> {
        self.stamp_for(grain_id).map(|stamp| stamp.epoch)
    }

    /// True only for the currently authoritative local incarnation.
    pub fn is_current(&self, grain_id: &GrainId, stamp: ActivationStamp) -> bool {
        self.stamp_for(grain_id) == Some(stamp)
    }

    pub fn grain_for(&self, handle: ActivationHandle) -> Option<&GrainId> {
        self.by_handle.get(&handle)
    }

    pub fn logical_id_for(&self, handle: ActivationHandle) -> Option<LogicalActorId> {
        self.grain_for(handle).map(GrainId::logical_id)
    }

    pub fn remove(&mut self, grain_id: &GrainId) -> Option<ActivationHandle> {
        let stamp = self.by_grain.remove(grain_id)?;
        self.by_handle.remove(&stamp.handle);
        Some(stamp.handle)
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
    fn activation_epoch_is_stable_while_live_and_advances_after_reactivation() {
        let mut directory = ActivationDirectory::new();
        let grain = GrainId::new("User", "epoch-test");

        let first = directory.resolve_or_activate(grain.clone()).unwrap();
        assert_eq!(first.epoch, ActivationEpoch::INITIAL);
        assert_eq!(directory.resolve_or_activate(grain.clone()).unwrap(), first);
        assert!(directory.is_current(&grain, first));

        assert_eq!(directory.remove(&grain), Some(first.handle));
        assert!(!directory.is_current(&grain, first));

        let second = directory.resolve_or_activate(grain.clone()).unwrap();
        assert_ne!(second.handle, first.handle);
        assert_eq!(second.epoch.get(), first.epoch.get() + 1);
        assert!(!directory.is_current(&grain, first));
        assert!(directory.is_current(&grain, second));
        assert_eq!(directory.epoch_for(&grain), Some(second.epoch));
    }

    #[test]
    fn activation_epochs_are_independent_per_full_logical_identity() {
        let mut directory = ActivationDirectory::new();
        let a = GrainId::new("User", "a");
        let b = GrainId::new("User", "b");

        let a1 = directory.resolve_or_activate(a.clone()).unwrap();
        let b1 = directory.resolve_or_activate(b.clone()).unwrap();
        directory.remove(&a);
        let a2 = directory.resolve_or_activate(a.clone()).unwrap();

        assert_eq!(b1.epoch, ActivationEpoch::INITIAL);
        assert_eq!(a2.epoch.get(), a1.epoch.get() + 1);
        assert_eq!(directory.epoch_for(&b), Some(ActivationEpoch::INITIAL));
    }

    #[test]
    fn external_epoch_fences_stale_local_activation_until_authority_is_installed() {
        let mut directory = ActivationDirectory::new();
        let grain = GrainId::new("User", "distributed-fence");

        let local = directory.resolve_or_activate(grain.clone()).unwrap();
        let epoch3 = ActivationEpoch::new(3).unwrap();
        assert_eq!(
            directory.observe_authoritative_epoch(grain.clone(), epoch3),
            Some(local)
        );
        assert!(!directory.is_current(&grain, local));
        assert_eq!(
            directory.resolve_or_activate(grain.clone()),
            Err(ActivationDirectoryError::AuthorityRequired)
        );

        let granted = directory
            .install_authoritative_activation(grain.clone(), epoch3)
            .unwrap();
        assert_eq!(granted.epoch, epoch3);
        assert_ne!(granted.handle, local.handle);
        assert!(directory.is_current(&grain, granted));
        assert_eq!(directory.resolve_or_activate(grain).unwrap(), granted);
    }

    #[test]
    fn stale_external_authority_grant_is_rejected() {
        let mut directory = ActivationDirectory::new();
        let grain = GrainId::new("User", "stale-grant");
        let epoch4 = ActivationEpoch::new(4).unwrap();
        directory.observe_authoritative_epoch(grain.clone(), epoch4);

        assert_eq!(
            directory.install_authoritative_activation(
                grain,
                ActivationEpoch::new(3).unwrap()
            ),
            Err(ActivationDirectoryError::StaleAuthority)
        );
    }

    #[test]
    fn stale_external_observation_does_not_fence_newer_authority() {
        let mut directory = ActivationDirectory::new();
        let grain = GrainId::new("User", "newer-authority");
        let epoch5 = ActivationEpoch::new(5).unwrap();
        let current = directory
            .install_authoritative_activation(grain.clone(), epoch5)
            .unwrap();

        assert_eq!(
            directory.observe_authoritative_epoch(
                grain.clone(),
                ActivationEpoch::new(4).unwrap()
            ),
            None
        );
        assert!(directory.is_current(&grain, current));
        assert_eq!(directory.resolve_or_activate(grain).unwrap(), current);
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
