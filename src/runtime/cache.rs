//! Packed, shard-local cache kernel for RESP-compatible coordination workloads.
//!
//! This module deliberately does **not** model cache entries as Nulang actors or
//! VM heap objects. A runtime shard owns one `CacheStore` and invokes it
//! directly on the shard thread. Cross-shard routing belongs above this layer.
//! The hot path therefore has no mailbox hop, scheduler yield, ORCA tracing, or
//! per-operation task allocation.
//!
//! The implementation is intentionally small but establishes the invariants the
//! production engine must preserve:
//! - compact inline values for small payloads;
//! - reusable size-class arena storage for larger keys/values;
//! - contiguous open-addressed indexing rather than pointer-heavy maps;
//! - generation-tagged expiry references so stale TTL work cannot delete a
//!   recycled slot;
//! - Redis Cluster compatible 16,384-slot hashing and hash tags.

use rustc_hash::{FxHashMap, FxHashSet, FxHasher};
use std::hash::Hasher;

pub const REDIS_CLUSTER_SLOTS: u16 = 16_384;
const INLINE_BYTES: usize = 22;
const EMPTY_SLOT: u32 = u32::MAX;
const TOMBSTONE_SLOT: u32 = u32::MAX - 1;
const MIN_ARENA_EXP: usize = 5; // 32 bytes
const MAX_ARENA_EXP: usize = 30; // 1 GiB blocks are the largest representable class
const FREE_LIST_COUNT: usize = MAX_ARENA_EXP - MIN_ARENA_EXP + 1;
const DEFAULT_INDEX_CAPACITY: usize = 64;
const DEFAULT_WHEEL_BUCKETS: usize = 4_096;
const DEFAULT_WHEEL_TICK_MS: u64 = 10;
const MAX_DURABLE_RESTORE_SLOTS: usize = 16_777_216;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ArenaSlice {
    offset: u32,
    len: u32,
    class: u8,
}

#[derive(Debug, Default)]
struct ByteArena {
    bytes: Vec<u8>,
    free: Vec<Vec<u32>>,
}

impl ByteArena {
    fn new() -> Self {
        Self {
            bytes: Vec::new(),
            free: (0..FREE_LIST_COUNT).map(|_| Vec::new()).collect(),
        }
    }

    fn class_for(len: usize) -> (usize, usize) {
        let capacity = len.max(1usize << MIN_ARENA_EXP).next_power_of_two();
        assert!(
            capacity <= (1usize << MAX_ARENA_EXP),
            "cache arena allocation exceeds 1 GiB"
        );
        let exp = capacity.trailing_zeros() as usize;
        (exp - MIN_ARENA_EXP, capacity)
    }

    fn alloc(&mut self, src: &[u8]) -> ArenaSlice {
        let (class, capacity) = Self::class_for(src.len());
        let offset = match self.free[class].pop() {
            Some(offset) => offset,
            None => {
                let offset = self.bytes.len();
                let end = offset
                    .checked_add(capacity)
                    .expect("cache arena address overflow");
                assert!(end <= u32::MAX as usize, "cache arena exceeds 4 GiB");
                self.bytes.resize(end, 0);
                offset as u32
            }
        };
        let start = offset as usize;
        self.bytes[start..start + src.len()].copy_from_slice(src);
        ArenaSlice {
            offset,
            len: src.len() as u32,
            class: class as u8,
        }
    }

    fn release(&mut self, slice: ArenaSlice) {
        self.free[slice.class as usize].push(slice.offset);
    }

    fn get(&self, slice: ArenaSlice) -> &[u8] {
        let start = slice.offset as usize;
        &self.bytes[start..start + slice.len as usize]
    }

    fn reserved_bytes(&self) -> usize {
        self.bytes.len()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PackedBytes {
    Inline { len: u8, data: [u8; INLINE_BYTES] },
    Arena(ArenaSlice),
}

impl PackedBytes {
    fn pack(src: &[u8], arena: &mut ByteArena) -> Self {
        if src.len() <= INLINE_BYTES {
            let mut data = [0u8; INLINE_BYTES];
            data[..src.len()].copy_from_slice(src);
            Self::Inline {
                len: src.len() as u8,
                data,
            }
        } else {
            Self::Arena(arena.alloc(src))
        }
    }

    fn as_slice<'a>(&'a self, arena: &'a ByteArena) -> &'a [u8] {
        match self {
            Self::Inline { len, data } => &data[..*len as usize],
            Self::Arena(slice) => arena.get(*slice),
        }
    }

    fn release(self, arena: &mut ByteArena) {
        if let Self::Arena(slice) = self {
            arena.release(slice);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheValueView<'a> {
    Integer(i64),
    Bytes(&'a [u8]),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheTtl {
    Missing,
    Persistent,
    RemainingMs(u64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheIncrementError {
    NotInteger,
    Overflow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CacheValue {
    Integer(i64),
    Bytes(PackedBytes),
}

impl CacheValue {
    fn view<'a>(&'a self, arena: &'a ByteArena) -> CacheValueView<'a> {
        match self {
            Self::Integer(value) => CacheValueView::Integer(*value),
            Self::Bytes(bytes) => CacheValueView::Bytes(bytes.as_slice(arena)),
        }
    }

    fn release(self, arena: &mut ByteArena) {
        if let Self::Bytes(bytes) = self {
            bytes.release(arena);
        }
    }
}

#[derive(Debug)]
struct Entry {
    hash: u64,
    key: PackedBytes,
    value: CacheValue,
    expires_at_ms: Option<u64>,
}

#[derive(Debug, Default)]
struct EntrySlot {
    generation: u32,
    entry: Option<Entry>,
}

#[derive(Debug, Clone, Copy)]
struct Bucket {
    hash: u64,
    slot: u32,
}

impl Bucket {
    const EMPTY: Self = Self {
        hash: 0,
        slot: EMPTY_SLOT,
    };

    fn is_live(self) -> bool {
        self.slot != EMPTY_SLOT && self.slot != TOMBSTONE_SLOT
    }
}

#[derive(Debug, Clone, Copy)]
struct ExpirationRef {
    slot: u32,
    generation: u32,
    expires_at_ms: u64,
}

#[derive(Debug)]
struct ExpirationWheel {
    tick_ms: u64,
    buckets: Vec<Vec<ExpirationRef>>,
    last_tick: Option<u64>,
}

impl ExpirationWheel {
    fn new(bucket_count: usize, tick_ms: u64) -> Self {
        assert!(bucket_count > 0);
        assert!(tick_ms > 0);
        Self {
            tick_ms,
            buckets: (0..bucket_count).map(|_| Vec::new()).collect(),
            last_tick: None,
        }
    }

    fn schedule(&mut self, item: ExpirationRef, now_ms: u64) {
        self.last_tick.get_or_insert(now_ms / self.tick_ms);
        let bucket = ((item.expires_at_ms / self.tick_ms) % self.buckets.len() as u64) as usize;
        self.buckets[bucket].push(item);
    }

    fn drain_candidates(&mut self, now_ms: u64, out: &mut Vec<ExpirationRef>) {
        let current = now_ms / self.tick_ms;
        let Some(last) = self.last_tick else {
            self.last_tick = Some(current);
            return;
        };

        let bucket_count = self.buckets.len() as u64;
        let elapsed = current.saturating_sub(last);

        if elapsed >= bucket_count {
            for bucket in &mut self.buckets {
                out.append(bucket);
            }
        } else {
            // Include the current bucket even when no full tick elapsed so
            // sub-tick TTLs can be reaped by an explicit purge call.
            // Revisit the previous tick as well. A sub-tick TTL may have
            // been scheduled into that bucket after the prior purge and can
            // become due before the clock advances into the next bucket.
            for tick in last..=current {
                let idx = (tick % bucket_count) as usize;
                out.append(&mut self.buckets[idx]);
            }
        }
        self.last_tick = Some(current);
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CacheStats {
    pub hits: u64,
    pub misses: u64,
    pub sets: u64,
    pub deletes: u64,
    pub expirations: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheMemoryStats {
    pub entries: usize,
    pub index_capacity: usize,
    pub arena_reserved_bytes: usize,
    pub reusable_slots: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheTransferToken {
    pub source_slot: u32,
    pub source_generation: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheTransferValue {
    Integer(i64),
    Bytes(Vec<u8>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheTransferEntry {
    pub key: Vec<u8>,
    pub value: CacheTransferValue,
    /// Remaining TTL at export time. None means persistent.
    pub ttl_ms: Option<u64>,
    pub token: CacheTransferToken,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheDurableEntry {
    pub key: Vec<u8>,
    pub value: CacheTransferValue,
    /// Process-independent absolute expiry. None means persistent.
    pub expires_unix_ms: Option<u64>,
    /// Exact source slot/generation identity. Preserving this across restore is
    /// required so pre-restart migration ACKs still fence the intended version.
    pub token: CacheTransferToken,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheDurableRestoreError {
    DuplicateSlot(u32),
    DuplicateKey,
    SlotOverflow,
    TooManySlots(usize),
    InvalidGeneration(u32),
}

impl CacheTransferEntry {
    pub fn payload_bytes(&self) -> usize {
        let value_bytes = match &self.value {
            CacheTransferValue::Integer(_) => std::mem::size_of::<i64>(),
            CacheTransferValue::Bytes(value) => value.len(),
        };
        self.key.len() + value_bytes
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheTransferCursor(pub usize);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheTransferBatch {
    pub slot: u16,
    pub entries: Vec<CacheTransferEntry>,
    pub next_cursor: Option<CacheTransferCursor>,
    pub scanned_slots: usize,
    pub payload_bytes: usize,
    pub exported_at_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheTransferFinalize {
    Removed,
    AlreadyAbsent,
    StaleVersion,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheTransferImport {
    Imported,
    AlreadyImported,
    ExpiredInTransit,
    Conflict,
    WrongSlot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CacheImportFence {
    source: CacheTransferToken,
    /// Target generation created by this source version. None records an
    /// accepted source version whose value had already expired in transit.
    target: Option<CacheTransferToken>,
    target_expires_at_ms: Option<u64>,
}

/// Migration-only target-side replay and conflict fencing.
///
/// This state is deliberately separate from every normal cache entry so the
/// steady-state memory layout pays no migration tax. Drop or clear the tracker
/// after its slot migration completes.
#[derive(Debug)]
pub struct CacheTransferImportTracker {
    slot: u16,
    fences: FxHashMap<Vec<u8>, CacheImportFence>,
}

impl CacheTransferImportTracker {
    pub fn new(slot: u16) -> Self {
        assert!(slot < REDIS_CLUSTER_SLOTS, "invalid Redis slot");
        Self {
            slot,
            fences: FxHashMap::default(),
        }
    }

    pub fn slot(&self) -> u16 {
        self.slot
    }

    pub fn len(&self) -> usize {
        self.fences.len()
    }

    pub fn is_empty(&self) -> bool {
        self.fences.is_empty()
    }

    pub fn clear(&mut self) {
        self.fences.clear();
    }

    pub fn import_entry(
        &mut self,
        store: &mut CacheStore,
        entry: &CacheTransferEntry,
        elapsed_ms: u64,
        now_ms: u64,
    ) -> CacheTransferImport {
        if redis_slot(&entry.key) != self.slot {
            return CacheTransferImport::WrongSlot;
        }

        let current = store.transfer_token_for_key(&entry.key, now_ms);
        let prior_fence = self.fences.get(&entry.key).copied();

        if let Some(fence) = prior_fence {
            if fence.source == entry.token {
                match fence.target {
                    Some(target) if current == Some(target) => {
                        return CacheTransferImport::AlreadyImported;
                    }
                    Some(_)
                        if current.is_none()
                            && fence
                                .target_expires_at_ms
                                .is_some_and(|deadline| deadline <= now_ms) =>
                    {
                        return CacheTransferImport::ExpiredInTransit;
                    }
                    None if current.is_none() => {
                        return CacheTransferImport::ExpiredInTransit;
                    }
                    _ => return CacheTransferImport::Conflict,
                }
            }

            if current != fence.target {
                return CacheTransferImport::Conflict;
            }
        } else if current.is_some() {
            // The target acquired this key outside this migration session.
            return CacheTransferImport::Conflict;
        }

        let remaining_ttl = entry.ttl_ms.map(|ttl| ttl.saturating_sub(elapsed_ms));
        if remaining_ttl == Some(0) {
            // A newer source version may expire while an older imported version
            // is still resident on the target. Leaving that older version would
            // resurrect stale data after source finalization. It is safe to
            // remove only when the current target generation still matches the
            // prior migration fence; the checks above enforce that condition.
            if current.is_some() {
                let removed = store.delete_at(&entry.key, now_ms);
                debug_assert!(removed, "fenced target entry vanished during expiry import");
            }
            self.fences.insert(
                entry.key.clone(),
                CacheImportFence {
                    source: entry.token,
                    target: None,
                    target_expires_at_ms: Some(now_ms),
                },
            );
            return CacheTransferImport::ExpiredInTransit;
        }

        let result = store.apply_transfer_entry(entry, elapsed_ms, now_ms);
        debug_assert_eq!(result, CacheTransferImport::Imported);
        if result != CacheTransferImport::Imported {
            return result;
        }

        let target = store
            .transfer_token_for_key(&entry.key, now_ms)
            .expect("imported transfer entry must have a live target token");
        let target_expires_at_ms = remaining_ttl.map(|ttl| now_ms.saturating_add(ttl));
        self.fences.insert(
            entry.key.clone(),
            CacheImportFence {
                source: entry.token,
                target: Some(target),
                target_expires_at_ms,
            },
        );
        CacheTransferImport::Imported
    }
}

/// Shard-local compact cache storage.
///
/// The owning runtime shard keeps `CacheStore` thread-confined by runtime
/// contract and calls it directly; the type does not require synchronization
/// internally.
#[derive(Debug)]
pub struct CacheStore {
    arena: ByteArena,
    slots: Vec<EntrySlot>,
    free_slots: Vec<u32>,
    index: Vec<Bucket>,
    index_len: usize,
    tombstones: usize,
    expiry: ExpirationWheel,
    expiry_scratch: Vec<ExpirationRef>,
    stats: CacheStats,
}

impl Default for CacheStore {
    fn default() -> Self {
        Self::new()
    }
}

impl CacheStore {
    pub fn new() -> Self {
        Self {
            arena: ByteArena::new(),
            slots: Vec::new(),
            free_slots: Vec::new(),
            index: vec![Bucket::EMPTY; DEFAULT_INDEX_CAPACITY],
            index_len: 0,
            tombstones: 0,
            expiry: ExpirationWheel::new(DEFAULT_WHEEL_BUCKETS, DEFAULT_WHEEL_TICK_MS),
            expiry_scratch: Vec::new(),
            stats: CacheStats::default(),
        }
    }

    #[inline]
    fn hash(key: &[u8]) -> u64 {
        let mut hasher = FxHasher::default();
        hasher.write(key);
        hasher.finish()
    }

    #[inline]
    fn find_slot(&self, key: &[u8], hash: u64) -> Option<u32> {
        let mask = self.index.len() - 1;
        let mut idx = hash as usize & mask;
        for _ in 0..self.index.len() {
            let bucket = self.index[idx];
            if bucket.slot == EMPTY_SLOT {
                return None;
            }
            if bucket.is_live() && bucket.hash == hash {
                let slot = &self.slots[bucket.slot as usize];
                if let Some(entry) = slot.entry.as_ref() {
                    if entry.key.as_slice(&self.arena) == key {
                        return Some(bucket.slot);
                    }
                }
            }
            idx = (idx + 1) & mask;
        }
        None
    }

    fn live_slot_id(&mut self, key: &[u8], now_ms: u64) -> Option<u32> {
        let hash = Self::hash(key);
        let slot_id = self.find_slot(key, hash)?;
        let expired = self.slots[slot_id as usize]
            .entry
            .as_ref()
            .and_then(|entry| entry.expires_at_ms)
            .is_some_and(|deadline| deadline <= now_ms);

        if expired {
            self.remove_slot(slot_id);
            self.stats.expirations += 1;
            None
        } else {
            Some(slot_id)
        }
    }

    fn ensure_index_capacity(&mut self) {
        if self.tombstones > self.index_len && self.tombstones > 32 {
            self.rehash(self.index.len());
        }

        let used = self.index_len + self.tombstones + 1;
        if used * 10 >= self.index.len() * 7 {
            self.rehash(self.index.len() * 2);
        }
    }

    fn rehash(&mut self, new_capacity: usize) {
        let capacity = new_capacity.max(DEFAULT_INDEX_CAPACITY).next_power_of_two();
        let old = std::mem::replace(&mut self.index, vec![Bucket::EMPTY; capacity]);
        self.tombstones = 0;
        for bucket in old.into_iter().filter(|bucket| bucket.is_live()) {
            self.insert_bucket_raw(bucket.hash, bucket.slot);
        }
    }

    fn insert_bucket_raw(&mut self, hash: u64, slot: u32) {
        let mask = self.index.len() - 1;
        let mut idx = hash as usize & mask;
        let mut first_tombstone = None;
        loop {
            let bucket = self.index[idx];
            if bucket.slot == TOMBSTONE_SLOT && first_tombstone.is_none() {
                first_tombstone = Some(idx);
            } else if bucket.slot == EMPTY_SLOT {
                let target = first_tombstone.unwrap_or(idx);
                if self.index[target].slot == TOMBSTONE_SLOT {
                    self.tombstones -= 1;
                }
                self.index[target] = Bucket { hash, slot };
                return;
            }
            idx = (idx + 1) & mask;
        }
    }

    fn remove_bucket(&mut self, hash: u64, slot: u32) {
        let mask = self.index.len() - 1;
        let mut idx = hash as usize & mask;
        for _ in 0..self.index.len() {
            let bucket = self.index[idx];
            if bucket.slot == EMPTY_SLOT {
                return;
            }
            if bucket.slot == slot && bucket.hash == hash {
                self.index[idx] = Bucket {
                    hash: 0,
                    slot: TOMBSTONE_SLOT,
                };
                self.index_len -= 1;
                self.tombstones += 1;
                return;
            }
            idx = (idx + 1) & mask;
        }
    }

    fn allocate_slot(&mut self, entry: Entry) -> (u32, u32) {
        if let Some(slot_id) = self.free_slots.pop() {
            let slot = &mut self.slots[slot_id as usize];
            slot.generation = slot.generation.wrapping_add(1).max(1);
            slot.entry = Some(entry);
            (slot_id, slot.generation)
        } else {
            let slot_id = self.slots.len() as u32;
            self.slots.push(EntrySlot {
                generation: 1,
                entry: Some(entry),
            });
            (slot_id, 1)
        }
    }

    fn remove_slot(&mut self, slot_id: u32) -> bool {
        let Some(slot) = self.slots.get_mut(slot_id as usize) else {
            return false;
        };
        let Some(entry) = slot.entry.take() else {
            return false;
        };
        self.remove_bucket(entry.hash, slot_id);
        entry.key.release(&mut self.arena);
        entry.value.release(&mut self.arena);
        self.free_slots.push(slot_id);
        true
    }

    fn set_value(&mut self, key: &[u8], value: CacheValue, ttl_ms: Option<u64>, now_ms: u64) {
        let hash = Self::hash(key);
        let expires_at_ms = ttl_ms.map(|ttl| now_ms.saturating_add(ttl));

        if let Some(slot_id) = self.find_slot(key, hash) {
            let slot = &mut self.slots[slot_id as usize];
            let entry = slot
                .entry
                .as_mut()
                .expect("live index points to live entry");
            let old_value = std::mem::replace(&mut entry.value, value);
            old_value.release(&mut self.arena);
            entry.expires_at_ms = expires_at_ms;
            slot.generation = slot.generation.wrapping_add(1).max(1);
            let generation = slot.generation;
            if let Some(expires_at_ms) = expires_at_ms {
                self.expiry.schedule(
                    ExpirationRef {
                        slot: slot_id,
                        generation,
                        expires_at_ms,
                    },
                    now_ms,
                );
            }
            self.stats.sets += 1;
            return;
        }

        self.ensure_index_capacity();
        let packed_key = PackedBytes::pack(key, &mut self.arena);
        let entry = Entry {
            hash,
            key: packed_key,
            value,
            expires_at_ms,
        };
        let (slot_id, generation) = self.allocate_slot(entry);
        self.insert_bucket_raw(hash, slot_id);
        self.index_len += 1;
        if let Some(expires_at_ms) = expires_at_ms {
            self.expiry.schedule(
                ExpirationRef {
                    slot: slot_id,
                    generation,
                    expires_at_ms,
                },
                now_ms,
            );
        }
        self.stats.sets += 1;
    }

    pub fn set_bytes(&mut self, key: &[u8], value: &[u8], ttl_ms: Option<u64>, now_ms: u64) {
        let value = CacheValue::Bytes(PackedBytes::pack(value, &mut self.arena));
        self.set_value(key, value, ttl_ms, now_ms);
    }

    pub fn set_integer(&mut self, key: &[u8], value: i64, ttl_ms: Option<u64>, now_ms: u64) {
        self.set_value(key, CacheValue::Integer(value), ttl_ms, now_ms);
    }

    pub fn get(&mut self, key: &[u8], now_ms: u64) -> Option<CacheValueView<'_>> {
        let Some(slot_id) = self.live_slot_id(key, now_ms) else {
            self.stats.misses += 1;
            return None;
        };

        self.stats.hits += 1;
        let entry = self.slots[slot_id as usize]
            .entry
            .as_ref()
            .expect("live slot vanished");
        Some(entry.value.view(&self.arena))
    }

    pub fn exists(&mut self, key: &[u8], now_ms: u64) -> bool {
        self.live_slot_id(key, now_ms).is_some()
    }

    pub fn delete_at(&mut self, key: &[u8], now_ms: u64) -> bool {
        let Some(slot_id) = self.live_slot_id(key, now_ms) else {
            return false;
        };
        let removed = self.remove_slot(slot_id);
        if removed {
            self.stats.deletes += 1;
        }
        removed
    }

    pub fn expire_ms(&mut self, key: &[u8], ttl_ms: u64, now_ms: u64) -> bool {
        let Some(slot_id) = self.live_slot_id(key, now_ms) else {
            return false;
        };

        if ttl_ms == 0 {
            if self.remove_slot(slot_id) {
                self.stats.expirations += 1;
                return true;
            }
            return false;
        }

        let expires_at_ms = now_ms.saturating_add(ttl_ms);
        let slot = &mut self.slots[slot_id as usize];
        let entry = slot.entry.as_mut().expect("live slot vanished");
        entry.expires_at_ms = Some(expires_at_ms);
        slot.generation = slot.generation.wrapping_add(1).max(1);
        let generation = slot.generation;
        self.expiry.schedule(
            ExpirationRef {
                slot: slot_id,
                generation,
                expires_at_ms,
            },
            now_ms,
        );
        true
    }

    pub fn ttl(&mut self, key: &[u8], now_ms: u64) -> CacheTtl {
        let Some(slot_id) = self.live_slot_id(key, now_ms) else {
            return CacheTtl::Missing;
        };
        let entry = self.slots[slot_id as usize]
            .entry
            .as_ref()
            .expect("live slot vanished");

        match entry.expires_at_ms {
            Some(deadline) => CacheTtl::RemainingMs(deadline.saturating_sub(now_ms)),
            None => CacheTtl::Persistent,
        }
    }

    pub fn increment(
        &mut self,
        key: &[u8],
        delta: i64,
        now_ms: u64,
    ) -> Result<i64, CacheIncrementError> {
        let Some(slot_id) = self.live_slot_id(key, now_ms) else {
            self.set_integer(key, delta, None, now_ms);
            return Ok(delta);
        };

        let current = self.slots[slot_id as usize]
            .entry
            .as_ref()
            .expect("live slot vanished")
            .value;

        let base = match current {
            CacheValue::Integer(value) => value,
            CacheValue::Bytes(bytes) => {
                let raw = bytes.as_slice(&self.arena);
                let text = std::str::from_utf8(raw).map_err(|_| CacheIncrementError::NotInteger)?;
                text.parse::<i64>()
                    .map_err(|_| CacheIncrementError::NotInteger)?
            }
        };

        let next = base
            .checked_add(delta)
            .ok_or(CacheIncrementError::Overflow)?;
        if let CacheValue::Bytes(bytes) = current {
            bytes.release(&mut self.arena);
        }
        let (generation, expires_at_ms) = {
            let slot = &mut self.slots[slot_id as usize];
            let entry = slot.entry.as_mut().expect("live slot vanished");
            entry.value = CacheValue::Integer(next);
            slot.generation = slot.generation.wrapping_add(1).max(1);
            (slot.generation, entry.expires_at_ms)
        };
        if let Some(expires_at_ms) = expires_at_ms {
            self.expiry.schedule(
                ExpirationRef {
                    slot: slot_id,
                    generation,
                    expires_at_ms,
                },
                now_ms,
            );
        }
        Ok(next)
    }

    pub fn delete(&mut self, key: &[u8]) -> bool {
        let hash = Self::hash(key);
        let Some(slot_id) = self.find_slot(key, hash) else {
            return false;
        };
        let removed = self.remove_slot(slot_id);
        if removed {
            self.stats.deletes += 1;
        }
        removed
    }

    pub fn purge_expired(&mut self, now_ms: u64, max_items: usize) -> usize {
        self.expiry_scratch.clear();
        self.expiry
            .drain_candidates(now_ms, &mut self.expiry_scratch);

        let mut expired = 0usize;
        let mut candidates = std::mem::take(&mut self.expiry_scratch);

        for item in candidates.iter().copied() {
            if expired >= max_items {
                self.expiry.schedule(item, now_ms);
                continue;
            }
            let Some(slot) = self.slots.get(item.slot as usize) else {
                continue;
            };
            if slot.generation != item.generation {
                continue;
            }
            let Some(entry) = slot.entry.as_ref() else {
                continue;
            };
            if entry.expires_at_ms != Some(item.expires_at_ms) {
                continue;
            }
            if item.expires_at_ms > now_ms {
                self.expiry.schedule(item, now_ms);
                continue;
            }
            if self.remove_slot(item.slot) {
                expired += 1;
                self.stats.expirations += 1;
            }
        }

        candidates.clear();
        self.expiry_scratch = candidates;
        expired
    }

    /// Export every live entry for a cold-path durable snapshot.
    ///
    /// wall_anchor_unix_ms should be captured before now_ms. Combining that
    /// earlier wall anchor with a later monotonic remaining TTL is conservative:
    /// an expiry can move slightly earlier after restore, never later.
    pub fn export_durable_entries(
        &self,
        now_ms: u64,
        wall_anchor_unix_ms: u64,
    ) -> Vec<CacheDurableEntry> {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(slot_id, slot)| {
                let entry = slot.entry.as_ref()?;
                if entry
                    .expires_at_ms
                    .is_some_and(|deadline| deadline <= now_ms)
                {
                    return None;
                }
                let key = entry.key.as_slice(&self.arena).to_vec();
                let value = match entry.value {
                    CacheValue::Integer(value) => CacheTransferValue::Integer(value),
                    CacheValue::Bytes(bytes) => {
                        CacheTransferValue::Bytes(bytes.as_slice(&self.arena).to_vec())
                    }
                };
                let expires_unix_ms = entry.expires_at_ms.map(|deadline| {
                    wall_anchor_unix_ms.saturating_add(deadline.saturating_sub(now_ms))
                });
                Some(CacheDurableEntry {
                    key,
                    value,
                    expires_unix_ms,
                    token: CacheTransferToken {
                        source_slot: slot_id as u32,
                        source_generation: slot.generation,
                    },
                })
            })
            .collect()
    }

    /// Restore a cold-path durable snapshot while preserving exact slot and
    /// generation identities.
    ///
    /// Entries already expired by wall_now_unix_ms are omitted. Their old slot
    /// generations are intentionally not reusable in this restored incarnation;
    /// free-slot reuse advances generation before any future allocation.
    pub fn restore_durable_entries(
        entries: &[CacheDurableEntry],
        now_ms: u64,
        wall_now_unix_ms: u64,
    ) -> Result<Self, CacheDurableRestoreError> {
        let mut live = Vec::new();
        let mut seen_slots = FxHashSet::default();
        let mut seen_keys = FxHashSet::default();
        let mut max_slot = None;

        for entry in entries {
            if entry
                .expires_unix_ms
                .is_some_and(|deadline| deadline <= wall_now_unix_ms)
            {
                continue;
            }
            if entry.token.source_generation == 0 {
                return Err(CacheDurableRestoreError::InvalidGeneration(0));
            }
            if !seen_slots.insert(entry.token.source_slot) {
                return Err(CacheDurableRestoreError::DuplicateSlot(
                    entry.token.source_slot,
                ));
            }
            if !seen_keys.insert(entry.key.clone()) {
                return Err(CacheDurableRestoreError::DuplicateKey);
            }
            max_slot = Some(max_slot.map_or(entry.token.source_slot, |current: u32| {
                current.max(entry.token.source_slot)
            }));
            live.push(entry);
        }

        let mut store = CacheStore::new();
        let Some(max_slot) = max_slot else {
            return Ok(store);
        };
        let slot_len = usize::try_from(max_slot)
            .ok()
            .and_then(|slot| slot.checked_add(1))
            .ok_or(CacheDurableRestoreError::SlotOverflow)?;
        if slot_len > MAX_DURABLE_RESTORE_SLOTS {
            return Err(CacheDurableRestoreError::TooManySlots(slot_len));
        }
        store.slots = (0..slot_len).map(|_| EntrySlot::default()).collect();
        store.free_slots.clear();
        store.index = vec![
            Bucket::EMPTY;
            (live.len().max(DEFAULT_INDEX_CAPACITY) * 2).next_power_of_two()
        ];
        store.index_len = 0;
        store.tombstones = 0;

        for durable in live {
            let slot_id = durable.token.source_slot;
            let hash = Self::hash(&durable.key);
            let packed_key = PackedBytes::pack(&durable.key, &mut store.arena);
            let value = match &durable.value {
                CacheTransferValue::Integer(value) => CacheValue::Integer(*value),
                CacheTransferValue::Bytes(bytes) => {
                    CacheValue::Bytes(PackedBytes::pack(bytes, &mut store.arena))
                }
            };
            let expires_at_ms = durable.expires_unix_ms.map(|deadline| {
                now_ms.saturating_add(deadline.saturating_sub(wall_now_unix_ms))
            });
            store.slots[slot_id as usize] = EntrySlot {
                generation: durable.token.source_generation,
                entry: Some(Entry {
                    hash,
                    key: packed_key,
                    value,
                    expires_at_ms,
                }),
            };
            store.insert_bucket_raw(hash, slot_id);
            store.index_len += 1;
            if let Some(expires_at_ms) = expires_at_ms {
                store.expiry.schedule(
                    ExpirationRef {
                        slot: slot_id,
                        generation: durable.token.source_generation,
                        expires_at_ms,
                    },
                    now_ms,
                );
            }
        }

        for (slot_id, slot) in store.slots.iter_mut().enumerate() {
            if slot.entry.is_none() {
                // Advance from zero on first reuse, ensuring no historical token
                // for an omitted/expired entry can match a future allocation.
                slot.generation = 0;
                store.free_slots.push(slot_id as u32);
            }
        }
        Ok(store)
    }

    /// Count live entries currently resident in one Redis logical slot.
    ///
    /// This is a cold migration-control scan and deliberately does not add a
    /// per-slot counter to the steady-state mutation path.
    pub fn live_entries_in_slot(&self, redis_slot_id: u16, now_ms: u64) -> usize {
        assert!(redis_slot_id < REDIS_CLUSTER_SLOTS, "invalid Redis slot");
        self.slots
            .iter()
            .filter_map(|slot| slot.entry.as_ref())
            .filter(|entry| {
                !entry
                    .expires_at_ms
                    .is_some_and(|deadline| deadline <= now_ms)
                    && redis_slot(entry.key.as_slice(&self.arena)) == redis_slot_id
            })
            .count()
    }

    /// Export a bounded batch of live entries in one Redis logical slot.
    ///
    /// The cursor is an opaque scan position over the source store's entry
    /// slots. Callers should restart from cursor zero after any stale
    /// finalization result so keys modified during an earlier pass are
    /// reconsidered.
    pub fn export_slot_batch(
        &mut self,
        redis_slot_id: u16,
        cursor: Option<CacheTransferCursor>,
        max_entries: usize,
        now_ms: u64,
    ) -> CacheTransferBatch {
        assert!(redis_slot_id < REDIS_CLUSTER_SLOTS, "invalid Redis slot");
        assert!(max_entries > 0, "transfer batch size must be non-zero");

        let mut index = cursor
            .map(|cursor| cursor.0)
            .unwrap_or(0)
            .min(self.slots.len());
        let start = index;
        let mut entries = Vec::with_capacity(max_entries);

        while index < self.slots.len() && entries.len() < max_entries {
            let slot_id = index as u32;
            let expired = self.slots[index]
                .entry
                .as_ref()
                .and_then(|entry| entry.expires_at_ms)
                .is_some_and(|deadline| deadline <= now_ms);

            if expired {
                if self.remove_slot(slot_id) {
                    self.stats.expirations += 1;
                }
                index += 1;
                continue;
            }

            let transfer = self.slots[index].entry.as_ref().and_then(|entry| {
                let key = entry.key.as_slice(&self.arena);
                if redis_slot(key) != redis_slot_id {
                    return None;
                }

                let value = match entry.value {
                    CacheValue::Integer(value) => CacheTransferValue::Integer(value),
                    CacheValue::Bytes(bytes) => {
                        CacheTransferValue::Bytes(bytes.as_slice(&self.arena).to_vec())
                    }
                };
                Some(CacheTransferEntry {
                    key: key.to_vec(),
                    value,
                    ttl_ms: entry
                        .expires_at_ms
                        .map(|deadline| deadline.saturating_sub(now_ms)),
                    token: CacheTransferToken {
                        source_slot: slot_id,
                        source_generation: self.slots[index].generation,
                    },
                })
            });

            if let Some(transfer) = transfer {
                entries.push(transfer);
            }
            index += 1;
        }

        let payload_bytes = entries.iter().map(CacheTransferEntry::payload_bytes).sum();
        CacheTransferBatch {
            slot: redis_slot_id,
            entries,
            next_cursor: (index < self.slots.len()).then_some(CacheTransferCursor(index)),
            scanned_slots: index.saturating_sub(start),
            payload_bytes,
            exported_at_ms: now_ms,
        }
    }

    /// Apply one transferred entry to the target shard.
    ///
    /// elapsed_ms allows a migration transport to subtract time spent in
    /// flight from a relative TTL. Passing zero is valid when the caller does
    /// not have a transit measurement.
    fn apply_transfer_entry(
        &mut self,
        entry: &CacheTransferEntry,
        elapsed_ms: u64,
        now_ms: u64,
    ) -> CacheTransferImport {
        let ttl_ms = match entry.ttl_ms {
            Some(ttl_ms) => {
                let remaining = ttl_ms.saturating_sub(elapsed_ms);
                if remaining == 0 {
                    return CacheTransferImport::ExpiredInTransit;
                }
                Some(remaining)
            }
            None => None,
        };

        match &entry.value {
            CacheTransferValue::Integer(value) => {
                self.set_integer(&entry.key, *value, ttl_ms, now_ms);
            }
            CacheTransferValue::Bytes(value) => {
                self.set_bytes(&entry.key, value, ttl_ms, now_ms);
            }
        }
        CacheTransferImport::Imported
    }

    fn transfer_token_for_key(&mut self, key: &[u8], now_ms: u64) -> Option<CacheTransferToken> {
        let slot_id = self.live_slot_id(key, now_ms)?;
        Some(CacheTransferToken {
            source_slot: slot_id,
            source_generation: self.slots[slot_id as usize].generation,
        })
    }

    /// Delete the source copy only if it is still exactly the version that was
    /// exported. A concurrent SET/INCR/EXPIRE changes the generation and turns
    /// the ACK into StaleVersion, forcing another reconciliation pass.
    pub fn finalize_transfer_entry(
        &mut self,
        entry: &CacheTransferEntry,
        now_ms: u64,
    ) -> CacheTransferFinalize {
        let hash = Self::hash(&entry.key);
        let Some(slot_id) = self.find_slot(&entry.key, hash) else {
            return CacheTransferFinalize::AlreadyAbsent;
        };

        let expired = self.slots[slot_id as usize]
            .entry
            .as_ref()
            .and_then(|current| current.expires_at_ms)
            .is_some_and(|deadline| deadline <= now_ms);
        if expired {
            if self.remove_slot(slot_id) {
                self.stats.expirations += 1;
            }
            return CacheTransferFinalize::AlreadyAbsent;
        }

        let current = &self.slots[slot_id as usize];
        if slot_id != entry.token.source_slot || current.generation != entry.token.source_generation
        {
            return CacheTransferFinalize::StaleVersion;
        }

        if self.remove_slot(slot_id) {
            CacheTransferFinalize::Removed
        } else {
            CacheTransferFinalize::AlreadyAbsent
        }
    }

    pub fn len(&self) -> usize {
        self.index_len
    }

    pub fn is_empty(&self) -> bool {
        self.index_len == 0
    }

    pub fn stats(&self) -> CacheStats {
        self.stats
    }

    pub fn memory_stats(&self) -> CacheMemoryStats {
        CacheMemoryStats {
            entries: self.index_len,
            index_capacity: self.index.len(),
            arena_reserved_bytes: self.arena.reserved_bytes(),
            reusable_slots: self.free_slots.len(),
        }
    }
}

/// Redis Cluster compatible key slot, including `{hash-tag}` semantics.
pub fn redis_slot(key: &[u8]) -> u16 {
    let hash_key = redis_hash_tag(key).unwrap_or(key);
    crc16_xmodem(hash_key) & (REDIS_CLUSTER_SLOTS - 1)
}

/// Default mapping from the stable Redis logical slot space onto physical
/// shard owners. Cluster placement may replace this with an explicit slot map.
pub fn default_physical_shard(slot: u16, shard_count: usize) -> usize {
    assert!(shard_count > 0, "cache shard count must be non-zero");
    slot as usize % shard_count
}

fn redis_hash_tag(key: &[u8]) -> Option<&[u8]> {
    let open = key.iter().position(|byte| *byte == b'{')?;
    let rest = &key[open + 1..];
    let close = rest.iter().position(|byte| *byte == b'}')?;
    if close == 0 {
        None
    } else {
        Some(&rest[..close])
    }
}

fn crc16_xmodem(bytes: &[u8]) -> u16 {
    let mut crc = 0u16;
    for byte in bytes {
        crc ^= (*byte as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_transfer_preserves_values_and_remaining_ttl() {
        let mut source = CacheStore::new();
        source.set_bytes(b"a{move}", b"bytes", Some(100), 10);
        source.set_integer(b"b{move}", 42, None, 10);
        let slot = redis_slot(b"a{move}");
        assert_eq!(redis_slot(b"b{move}"), slot);

        let batch = source.export_slot_batch(slot, None, 16, 30);
        assert_eq!(batch.entries.len(), 2);
        assert_eq!(batch.next_cursor, None);
        assert!(batch.payload_bytes > 0);

        let mut target = CacheStore::new();
        let mut imports = CacheTransferImportTracker::new(slot);
        for entry in &batch.entries {
            assert_eq!(
                imports.import_entry(&mut target, entry, 5, 1000),
                CacheTransferImport::Imported
            );
        }

        assert_eq!(
            target.get(b"a{move}", 1000),
            Some(CacheValueView::Bytes(b"bytes"))
        );
        assert_eq!(
            target.get(b"b{move}", 1000),
            Some(CacheValueView::Integer(42))
        );
        assert_eq!(target.ttl(b"a{move}", 1000), CacheTtl::RemainingMs(75));
        assert_eq!(target.ttl(b"b{move}", 1000), CacheTtl::Persistent);
    }

    #[test]
    fn stale_transfer_ack_cannot_delete_raced_set() {
        let mut source = CacheStore::new();
        source.set_bytes(b"k{move}", b"old", None, 0);
        let slot = redis_slot(b"k{move}");
        let batch = source.export_slot_batch(slot, None, 1, 0);
        let entry = batch.entries.first().unwrap().clone();

        source.set_bytes(b"k{move}", b"new", None, 1);

        assert_eq!(
            source.finalize_transfer_entry(&entry, 1),
            CacheTransferFinalize::StaleVersion
        );
        assert_eq!(
            source.get(b"k{move}", 1),
            Some(CacheValueView::Bytes(b"new"))
        );
    }

    #[test]
    fn exact_transfer_ack_removes_source_copy_idempotently() {
        let mut source = CacheStore::new();
        source.set_integer(b"k{move}", 7, None, 0);
        let slot = redis_slot(b"k{move}");
        let entry = source
            .export_slot_batch(slot, None, 1, 0)
            .entries
            .into_iter()
            .next()
            .unwrap();

        assert_eq!(
            source.finalize_transfer_entry(&entry, 0),
            CacheTransferFinalize::Removed
        );
        assert_eq!(
            source.finalize_transfer_entry(&entry, 0),
            CacheTransferFinalize::AlreadyAbsent
        );
        assert!(!source.exists(b"k{move}", 0));
    }

    #[test]
    fn increment_invalidates_transfer_token_without_losing_ttl_expiry() {
        let mut source = CacheStore::new();
        source.set_integer(b"k{move}", 1, Some(50), 0);
        let slot = redis_slot(b"k{move}");
        let entry = source
            .export_slot_batch(slot, None, 1, 0)
            .entries
            .into_iter()
            .next()
            .unwrap();

        assert_eq!(source.increment(b"k{move}", 1, 10).unwrap(), 2);
        assert_eq!(
            source.finalize_transfer_entry(&entry, 10),
            CacheTransferFinalize::StaleVersion
        );

        assert_eq!(source.purge_expired(60, 16), 1);
        assert!(!source.exists(b"k{move}", 60));
    }

    #[test]
    fn slot_transfer_batches_are_bounded_and_resumable() {
        let mut source = CacheStore::new();
        for key in [b"a{batch}".as_slice(), b"b{batch}", b"c{batch}"] {
            source.set_bytes(key, b"value", None, 0);
        }
        let slot = redis_slot(b"a{batch}");

        let first = source.export_slot_batch(slot, None, 2, 0);
        assert_eq!(first.entries.len(), 2);
        let second = source.export_slot_batch(slot, first.next_cursor, 2, 0);
        assert_eq!(second.entries.len(), 1);
        assert_eq!(second.next_cursor, None);
    }

    #[test]
    fn repeated_import_is_idempotent_and_does_not_refresh_ttl() {
        let mut source = CacheStore::new();
        source.set_bytes(b"k{move}", b"value", Some(100), 0);
        let slot = redis_slot(b"k{move}");
        let entry = source
            .export_slot_batch(slot, None, 1, 0)
            .entries
            .into_iter()
            .next()
            .unwrap();

        let mut target = CacheStore::new();
        let mut imports = CacheTransferImportTracker::new(slot);
        assert_eq!(
            imports.import_entry(&mut target, &entry, 0, 1000),
            CacheTransferImport::Imported
        );
        assert_eq!(target.ttl(b"k{move}", 1020), CacheTtl::RemainingMs(80));

        assert_eq!(
            imports.import_entry(&mut target, &entry, 0, 1020),
            CacheTransferImport::AlreadyImported
        );
        assert_eq!(target.ttl(b"k{move}", 1020), CacheTtl::RemainingMs(80));
    }

    #[test]
    fn replayed_transfer_cannot_overwrite_target_side_write() {
        let mut source = CacheStore::new();
        source.set_bytes(b"k{move}", b"source", None, 0);
        let slot = redis_slot(b"k{move}");
        let entry = source
            .export_slot_batch(slot, None, 1, 0)
            .entries
            .into_iter()
            .next()
            .unwrap();

        let mut target = CacheStore::new();
        let mut imports = CacheTransferImportTracker::new(slot);
        assert_eq!(
            imports.import_entry(&mut target, &entry, 0, 0),
            CacheTransferImport::Imported
        );

        target.set_bytes(b"k{move}", b"client", None, 1);
        assert_eq!(
            imports.import_entry(&mut target, &entry, 0, 1),
            CacheTransferImport::Conflict
        );
        assert_eq!(
            target.get(b"k{move}", 1),
            Some(CacheValueView::Bytes(b"client"))
        );
    }

    #[test]
    fn newer_source_version_replaces_unchanged_prior_import() {
        let mut source = CacheStore::new();
        source.set_bytes(b"k{move}", b"old", None, 0);
        let slot = redis_slot(b"k{move}");
        let first = source
            .export_slot_batch(slot, None, 1, 0)
            .entries
            .into_iter()
            .next()
            .unwrap();

        let mut target = CacheStore::new();
        let mut imports = CacheTransferImportTracker::new(slot);
        assert_eq!(
            imports.import_entry(&mut target, &first, 0, 0),
            CacheTransferImport::Imported
        );

        source.set_bytes(b"k{move}", b"new", None, 1);
        let second = source
            .export_slot_batch(slot, None, 1, 1)
            .entries
            .into_iter()
            .next()
            .unwrap();
        assert_ne!(first.token, second.token);

        assert_eq!(
            imports.import_entry(&mut target, &second, 0, 1),
            CacheTransferImport::Imported
        );
        assert_eq!(
            target.get(b"k{move}", 1),
            Some(CacheValueView::Bytes(b"new"))
        );
    }

    #[test]
    fn import_tracker_rejects_entry_from_another_slot() {
        let mut source = CacheStore::new();
        source.set_bytes(b"k{other}", b"value", None, 0);
        let other_slot = redis_slot(b"k{other}");
        let entry = source
            .export_slot_batch(other_slot, None, 1, 0)
            .entries
            .into_iter()
            .next()
            .unwrap();

        let expected_slot = (other_slot + 1) % REDIS_CLUSTER_SLOTS;
        let mut target = CacheStore::new();
        let mut imports = CacheTransferImportTracker::new(expected_slot);
        assert_eq!(
            imports.import_entry(&mut target, &entry, 0, 0),
            CacheTransferImport::WrongSlot
        );
        assert!(target.is_empty());
    }

    #[test]
    fn live_slot_count_tracks_fenced_transfer_finalization() {
        let mut source = CacheStore::new();
        source.set_bytes(b"a{count}", b"one", None, 0);
        source.set_bytes(b"b{count}", b"two", None, 0);
        let slot = redis_slot(b"a{count}");
        assert_eq!(source.live_entries_in_slot(slot, 0), 2);

        let batch = source.export_slot_batch(slot, None, 8, 0);
        assert_eq!(
            source.finalize_transfer_entry(&batch.entries[0], 0),
            CacheTransferFinalize::Removed
        );
        assert_eq!(source.live_entries_in_slot(slot, 0), 1);
        assert_eq!(
            source.finalize_transfer_entry(&batch.entries[1], 0),
            CacheTransferFinalize::Removed
        );
        assert_eq!(source.live_entries_in_slot(slot, 0), 0);
    }

    #[test]
    fn newer_expired_source_version_removes_older_fenced_target_value() {
        let mut source = CacheStore::new();
        source.set_bytes(b"k{move}", b"old", None, 0);
        let slot = redis_slot(b"k{move}");
        let first = source
            .export_slot_batch(slot, None, 1, 0)
            .entries
            .into_iter()
            .next()
            .unwrap();

        let mut target = CacheStore::new();
        let mut imports = CacheTransferImportTracker::new(slot);
        assert_eq!(
            imports.import_entry(&mut target, &first, 0, 0),
            CacheTransferImport::Imported
        );
        assert_eq!(
            target.get(b"k{move}", 0),
            Some(CacheValueView::Bytes(b"old"))
        );

        source.set_bytes(b"k{move}", b"new", Some(5), 1);
        let second = source
            .export_slot_batch(slot, None, 1, 1)
            .entries
            .into_iter()
            .next()
            .unwrap();
        assert_ne!(first.token, second.token);

        assert_eq!(
            imports.import_entry(&mut target, &second, 5, 6),
            CacheTransferImport::ExpiredInTransit
        );
        assert!(!target.exists(b"k{move}", 6));

        // Replaying the exact expired source version is idempotent and cannot
        // resurrect the older imported value.
        assert_eq!(
            imports.import_entry(&mut target, &second, 5, 7),
            CacheTransferImport::ExpiredInTransit
        );
        assert!(!target.exists(b"k{move}", 7));
    }

    #[test]
    fn transfer_import_does_not_resurrect_expired_in_transit_key() {
        let mut source = CacheStore::new();
        source.set_bytes(b"k{move}", b"value", Some(5), 0);
        let slot = redis_slot(b"k{move}");
        let entry = source
            .export_slot_batch(slot, None, 1, 0)
            .entries
            .into_iter()
            .next()
            .unwrap();
        let mut target = CacheStore::new();
        let mut imports = CacheTransferImportTracker::new(slot);

        assert_eq!(
            imports.import_entry(&mut target, &entry, 5, 100),
            CacheTransferImport::ExpiredInTransit
        );
        assert!(!target.exists(b"k{move}", 100));
    }

    #[test]
    fn durable_snapshot_restore_preserves_source_tokens_and_ttl() {
        let mut store = CacheStore::new();
        store.set_bytes(b"persistent", b"value", None, 100);
        store.set_integer(b"ttl", 42, Some(5_000), 100);

        let persistent_token = store.transfer_token_for_key(b"persistent", 200).unwrap();
        let ttl_token = store.transfer_token_for_key(b"ttl", 200).unwrap();
        let snapshot = store.export_durable_entries(200, 10_000);

        let mut restored =
            CacheStore::restore_durable_entries(&snapshot, 50, 11_000).unwrap();
        assert_eq!(
            restored.get(b"persistent", 50),
            Some(CacheValueView::Bytes(b"value"))
        );
        assert_eq!(restored.get(b"ttl", 50), Some(CacheValueView::Integer(42)));
        assert_eq!(
            restored.transfer_token_for_key(b"persistent", 50),
            Some(persistent_token)
        );
        assert_eq!(
            restored.transfer_token_for_key(b"ttl", 50),
            Some(ttl_token)
        );
        assert_eq!(restored.ttl(b"ttl", 50), CacheTtl::RemainingMs(3_900));
    }

    #[test]
    fn durable_snapshot_restore_drops_wall_expired_entries() {
        let mut store = CacheStore::new();
        store.set_bytes(b"short", b"value", Some(100), 0);
        let snapshot = store.export_durable_entries(10, 1_000);
        let restored = CacheStore::restore_durable_entries(&snapshot, 0, 1_200).unwrap();
        assert!(restored.is_empty());
    }

    #[test]
    fn restored_source_token_still_generation_fences_old_transfer_ack() {
        let mut source = CacheStore::new();
        source.set_bytes(b"k{durable}", b"old", None, 0);
        let slot = redis_slot(b"k{durable}");
        let exported = source
            .export_slot_batch(slot, None, 1, 0)
            .entries
            .into_iter()
            .next()
            .unwrap();
        let snapshot = source.export_durable_entries(0, 10_000);

        let mut restored =
            CacheStore::restore_durable_entries(&snapshot, 0, 10_100).unwrap();
        assert_eq!(
            restored.finalize_transfer_entry(&exported, 0),
            CacheTransferFinalize::Removed
        );

        let mut restored =
            CacheStore::restore_durable_entries(&snapshot, 0, 10_100).unwrap();
        restored.set_bytes(b"k{durable}", b"new", None, 1);
        assert_eq!(
            restored.finalize_transfer_entry(&exported, 1),
            CacheTransferFinalize::StaleVersion
        );
        assert_eq!(
            restored.get(b"k{durable}", 1),
            Some(CacheValueView::Bytes(b"new"))
        );
    }

    #[test]
    fn durable_restore_rejects_sparse_allocation_bomb_and_zero_generation() {
        let huge = CacheDurableEntry {
            key: b"huge".to_vec(),
            value: CacheTransferValue::Integer(1),
            expires_unix_ms: None,
            token: CacheTransferToken {
                source_slot: u32::MAX,
                source_generation: 1,
            },
        };
        assert!(matches!(
            CacheStore::restore_durable_entries(&[huge], 0, 0),
            Err(CacheDurableRestoreError::TooManySlots(_))
                | Err(CacheDurableRestoreError::SlotOverflow)
        ));

        let zero_generation = CacheDurableEntry {
            key: b"zero".to_vec(),
            value: CacheTransferValue::Integer(1),
            expires_unix_ms: None,
            token: CacheTransferToken {
                source_slot: 0,
                source_generation: 0,
            },
        };
        assert!(matches!(
            CacheStore::restore_durable_entries(&[zero_generation], 0, 0),
            Err(CacheDurableRestoreError::InvalidGeneration(0))
        ));
    }

    #[test]
    fn inline_values_do_not_touch_arena() {
        let mut store = CacheStore::new();
        store.set_bytes(b"k", b"small", None, 0);
        assert_eq!(store.memory_stats().arena_reserved_bytes, 0);
        assert_eq!(store.get(b"k", 0), Some(CacheValueView::Bytes(b"small")));
    }

    #[test]
    fn large_value_blocks_are_reused_after_delete() {
        let mut store = CacheStore::new();
        let a = vec![1u8; 100];
        let b = vec![2u8; 100];
        store.set_bytes(b"a", &a, None, 0);
        let reserved = store.memory_stats().arena_reserved_bytes;
        assert_eq!(reserved, 128);
        assert!(store.delete(b"a"));
        store.set_bytes(b"b", &b, None, 0);
        assert_eq!(store.memory_stats().arena_reserved_bytes, reserved);
        assert_eq!(
            store.get(b"b", 0),
            Some(CacheValueView::Bytes(b.as_slice()))
        );
    }

    #[test]
    fn ttl_is_lazy_and_wheel_reaped() {
        let mut store = CacheStore::new();
        store.set_integer(b"a", 7, Some(5), 100);
        assert_eq!(store.get(b"a", 104), Some(CacheValueView::Integer(7)));
        assert_eq!(store.get(b"a", 105), None);

        store.set_integer(b"b", 9, Some(10), 200);
        assert_eq!(store.purge_expired(210, 100), 1);
        assert_eq!(store.get(b"b", 210), None);
    }

    #[test]
    fn sub_tick_expiry_is_reaped_after_tick_advance() {
        let mut store = CacheStore::new();
        store.set_integer(b"short", 1, Some(5), 100);
        assert_eq!(store.purge_expired(110, 100), 1);
        assert!(store.is_empty());
    }

    #[test]
    fn delete_churn_compacts_tombstones_before_growing_index() {
        let mut store = CacheStore::new();
        for i in 0..48u64 {
            let key = i.to_le_bytes();
            store.set_integer(&key, i as i64, None, 0);
        }
        let capacity = store.memory_stats().index_capacity;

        for i in 0..40u64 {
            let key = i.to_le_bytes();
            assert!(store.delete(&key));
        }

        for i in 100..132u64 {
            let key = i.to_le_bytes();
            store.set_integer(&key, i as i64, None, 0);
        }

        assert_eq!(store.memory_stats().index_capacity, capacity);
    }

    #[test]
    fn expire_and_ttl_preserve_live_key_semantics() {
        let mut store = CacheStore::new();
        store.set_bytes(b"k", b"v", None, 100);
        assert_eq!(store.ttl(b"k", 100), CacheTtl::Persistent);
        assert!(store.expire_ms(b"k", 2_500, 100));
        assert_eq!(store.ttl(b"k", 600), CacheTtl::RemainingMs(2_000));
        assert_eq!(store.ttl(b"k", 2_600), CacheTtl::Missing);
    }

    #[test]
    fn increment_promotes_bytes_and_preserves_ttl() {
        let mut store = CacheStore::new();
        store.set_bytes(b"n", b"41", Some(5_000), 100);
        assert_eq!(store.increment(b"n", 1, 200), Ok(42));
        assert_eq!(store.get(b"n", 200), Some(CacheValueView::Integer(42)));
        assert_eq!(store.ttl(b"n", 200), CacheTtl::RemainingMs(4_900));
    }

    #[test]
    fn increment_rejects_non_integer_and_overflow() {
        let mut store = CacheStore::new();
        store.set_bytes(b"text", b"nope", None, 0);
        assert_eq!(
            store.increment(b"text", 1, 0),
            Err(CacheIncrementError::NotInteger)
        );
        store.set_integer(b"max", i64::MAX, None, 0);
        assert_eq!(
            store.increment(b"max", 1, 0),
            Err(CacheIncrementError::Overflow)
        );
    }

    #[test]
    fn delete_at_treats_expired_keys_as_missing() {
        let mut store = CacheStore::new();
        store.set_bytes(b"k", b"v", Some(5), 100);
        assert!(!store.delete_at(b"k", 105));
        assert!(store.is_empty());
    }

    #[test]
    fn stale_expiry_cannot_delete_recycled_slot() {
        let mut store = CacheStore::new();
        store.set_integer(b"old", 1, Some(10), 0);
        assert!(store.delete(b"old"));
        store.set_integer(b"new", 2, Some(1_000), 0);
        store.purge_expired(10, 100);
        assert_eq!(store.get(b"new", 10), Some(CacheValueView::Integer(2)));
    }

    #[test]
    fn redis_slot_matches_xmodem_vector() {
        // CRC16/XMODEM("123456789") == 0x31c3 and is below 16384.
        assert_eq!(redis_slot(b"123456789"), 0x31c3);
    }

    #[test]
    fn redis_hash_tags_colocate_related_keys() {
        assert_eq!(
            redis_slot(b"user:{42}:profile"),
            redis_slot(b"user:{42}:sessions")
        );
        assert_eq!(redis_slot(b"foo{bar}zap"), redis_slot(b"bar"));
    }

    #[test]
    fn physical_shard_is_deterministic() {
        let slot = redis_slot(b"tenant:{9}:x");
        assert_eq!(
            default_physical_shard(slot, 8),
            default_physical_shard(slot, 8)
        );
    }
}
