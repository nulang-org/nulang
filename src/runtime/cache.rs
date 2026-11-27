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

use rustc_hash::FxHasher;
use std::collections::{HashSet, VecDeque};
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
    Admission(CacheAdmissionError),
}

pub(super) fn parse_cache_integer(value: CacheValueView<'_>) -> Result<i64, CacheIncrementError> {
    match value {
        CacheValueView::Integer(value) => Ok(value),
        CacheValueView::Bytes(raw) => {
            // RESP canonical decimal syntax.
            if raw != b"0" && !matches!(raw, [b'1'..=b'9', ..] | [b'-', b'1'..=b'9', ..]) {
                return Err(CacheIncrementError::NotInteger);
            }
            std::str::from_utf8(raw)
                .map_err(|_| CacheIncrementError::NotInteger)?
                .parse::<i64>()
                .map_err(|_| CacheIncrementError::NotInteger)
        }
    }
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

// Slot-indexed intrusive FIFO lists: refresh/delete unlinks one record
// without scanning a bucket or moving other records.
#[derive(Debug, Clone, Copy)]
struct ExpirationNode {
    item: ExpirationRef,
    bucket: usize,
    prev: Option<u32>,
    next: Option<u32>,
}

#[derive(Debug, Default)]
struct ExpirationBucket {
    head: Option<u32>,
    tail: Option<u32>,
}

#[derive(Debug)]
struct ExpirationWheel {
    tick_ms: u64,
    buckets: Vec<ExpirationBucket>,
    nodes: Vec<Option<ExpirationNode>>,
    pending: VecDeque<usize>,
    queued: Vec<bool>,
    last_tick: Option<u64>,
}

impl ExpirationWheel {
    fn new(bucket_count: usize, tick_ms: u64) -> Self {
        assert!(bucket_count > 0);
        assert!(tick_ms > 0);
        Self {
            tick_ms,
            buckets: (0..bucket_count)
                .map(|_| ExpirationBucket::default())
                .collect(),
            nodes: Vec::new(),
            pending: VecDeque::new(),
            queued: vec![false; bucket_count],
            last_tick: None,
        }
    }

    fn cancel(&mut self, slot: u32) -> Option<ExpirationRef> {
        let node = self.nodes.get_mut(slot as usize)?.take()?;
        let bucket = &mut self.buckets[node.bucket];
        if let Some(prev) = node.prev {
            self.nodes[prev as usize]
                .as_mut()
                .expect("linked expiration")
                .next = node.next;
        } else {
            bucket.head = node.next;
        }
        if let Some(next) = node.next {
            self.nodes[next as usize]
                .as_mut()
                .expect("linked expiration")
                .prev = node.prev;
        } else {
            bucket.tail = node.prev;
        }
        Some(node.item)
    }

    fn schedule(&mut self, item: ExpirationRef, now_ms: u64) {
        self.cancel(item.slot);
        self.last_tick.get_or_insert(now_ms / self.tick_ms);
        let idx = ((item.expires_at_ms / self.tick_ms) % self.buckets.len() as u64) as usize;
        let slot = item.slot as usize;
        if self.nodes.len() <= slot {
            self.nodes.resize(slot + 1, None);
        }
        let bucket = &mut self.buckets[idx];
        let prev = bucket.tail;
        if let Some(prev) = prev {
            self.nodes[prev as usize]
                .as_mut()
                .expect("linked expiration")
                .next = Some(item.slot);
        } else {
            bucket.head = Some(item.slot);
        }
        bucket.tail = Some(item.slot);
        self.nodes[slot] = Some(ExpirationNode {
            item,
            bucket: idx,
            prev,
            next: None,
        });
    }

    fn enqueue_bucket(&mut self, idx: usize) {
        if !self.queued[idx] && self.buckets[idx].head.is_some() {
            self.queued[idx] = true;
            self.pending.push_back(idx);
        }
    }

    /// Retain eligible buckets across calls and inspect at most the supplied
    /// number of records, including stale and not-yet-due records.
    fn drain_candidates(&mut self, now_ms: u64, out: &mut Vec<ExpirationRef>, budget: usize) {
        if budget == 0 {
            return;
        }
        let current = now_ms / self.tick_ms;
        let Some(last) = self.last_tick else {
            self.last_tick = Some(current);
            return;
        };
        let bucket_count = self.buckets.len() as u64;
        let elapsed = current.saturating_sub(last);
        if elapsed >= bucket_count {
            for idx in 0..self.buckets.len() {
                self.enqueue_bucket(idx);
            }
        } else {
            // Revisit the previous and current ticks for sub-tick TTLs.
            for tick in last..=current {
                self.enqueue_bucket((tick % bucket_count) as usize);
            }
        }
        self.last_tick = Some(current);

        // Each bucket receives one turn per pass. FIFO records prevent a
        // rescheduled future collision from repeatedly hiding older entries.
        let initial_len = out.len();
        while out.len() - initial_len < budget {
            let Some(idx) = self.pending.pop_front() else {
                break;
            };
            self.queued[idx] = false;
            while out.len() - initial_len < budget {
                let Some(slot) = self.buckets[idx].head else {
                    break;
                };
                out.push(self.cancel(slot).expect("linked expiration"));
            }
            self.enqueue_bucket(idx);
        }
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

/// Bounds physical entry count and initialized size-class arena extent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheLimits {
    pub max_entries: usize,
    pub max_arena_bytes: usize,
}

impl Default for CacheLimits {
    fn default() -> Self {
        Self {
            max_entries: 1_000_000,
            max_arena_bytes: 256 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheAdmissionError {
    EntryLimit,
    ArenaLimit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheMemoryStats {
    pub entries: usize,
    pub index_capacity: usize,
    pub arena_reserved_bytes: usize,
    /// Approximate retained cache allocations, excluding allocator overhead and temporary command buffers.
    pub retained_capacity_bytes: usize,
    pub reusable_slots: usize,
}

/// Shard-local compact cache storage.
///
/// The owning runtime shard keeps `CacheStore` thread-confined by runtime
/// contract and calls it directly; the type does not require synchronization
/// internally.
#[derive(Debug)]
pub struct CacheStore {
    limits: CacheLimits,
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
        Self::with_limits(CacheLimits::default())
    }

    pub fn with_limits(limits: CacheLimits) -> Self {
        Self {
            limits,
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
        self.expiry.cancel(slot_id);
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
            } else {
                self.expiry.cancel(slot_id);
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

    fn admit_block(
        &self,
        len: usize,
        free: &mut [usize; FREE_LIST_COUNT],
        extent: &mut usize,
    ) -> Result<(), CacheAdmissionError> {
        if len <= INLINE_BYTES {
            return Ok(());
        }
        if len > (1usize << MAX_ARENA_EXP) {
            return Err(CacheAdmissionError::ArenaLimit);
        }
        let (class, capacity) = ByteArena::class_for(len);
        if free[class] > 0 {
            free[class] -= 1;
        } else {
            *extent = extent
                .checked_add(capacity)
                .ok_or(CacheAdmissionError::ArenaLimit)?;
            if *extent > self.limits.max_arena_bytes.min(u32::MAX as usize) {
                return Err(CacheAdmissionError::ArenaLimit);
            }
        }
        Ok(())
    }

    fn admit_entries(&self, new_entries: usize) -> Result<(), CacheAdmissionError> {
        if new_entries
            > self
                .limits
                .max_entries
                .min(TOMBSTONE_SLOT as usize)
                .saturating_sub(self.index_len)
        {
            return Err(CacheAdmissionError::EntryLimit);
        }
        Ok(())
    }

    pub(super) fn admit_single(
        &self,
        key: &[u8],
        value_len: usize,
    ) -> Result<(), CacheAdmissionError> {
        let new_key = self.find_slot(key, Self::hash(key)).is_none();
        self.admit_entries(usize::from(new_key))?;
        if value_len <= INLINE_BYTES && (!new_key || key.len() <= INLINE_BYTES) {
            return Ok(());
        }
        let mut free = std::array::from_fn(|i| self.arena.free[i].len());
        let mut extent = self.arena.bytes.len();
        self.admit_block(value_len, &mut free, &mut extent)?;
        if new_key {
            self.admit_block(key.len(), &mut free, &mut extent)?;
        }
        Ok(())
    }

    pub fn set_bytes(
        &mut self,
        key: &[u8],
        value: &[u8],
        ttl_ms: Option<u64>,
        now_ms: u64,
    ) -> Result<(), CacheAdmissionError> {
        self.admit_single(key, value.len())?;
        self.set_bytes_admitted(key, value, ttl_ms, now_ms);
        Ok(())
    }

    fn set_bytes_admitted(&mut self, key: &[u8], value: &[u8], ttl_ms: Option<u64>, now_ms: u64) {
        let value = CacheValue::Bytes(PackedBytes::pack(value, &mut self.arena));
        self.set_value(key, value, ttl_ms, now_ms);
    }

    /// Preflight the complete batch before any mutation. Conservative: blocks
    /// freed during the batch are not counted as available for admission.
    pub fn set_batch(
        &mut self,
        pairs: &[(&[u8], &[u8])],
        now_ms: u64,
    ) -> Result<(), CacheAdmissionError> {
        self.admit_batch(pairs)?;
        for &(key, value) in pairs {
            self.set_bytes_admitted(key, value, None, now_ms);
        }
        Ok(())
    }

    pub(super) fn admit_batch(&self, pairs: &[(&[u8], &[u8])]) -> Result<(), CacheAdmissionError> {
        let mut new_keys = HashSet::new();
        let mut free = std::array::from_fn(|i| self.arena.free[i].len());
        let mut extent = self.arena.bytes.len();
        for &(key, value) in pairs {
            self.admit_block(value.len(), &mut free, &mut extent)?;
            if self.find_slot(key, Self::hash(key)).is_none() && new_keys.insert(key) {
                self.admit_entries(new_keys.len())?;
                self.admit_block(key.len(), &mut free, &mut extent)?;
            }
        }
        Ok(())
    }

    pub fn set_integer(
        &mut self,
        key: &[u8],
        value: i64,
        ttl_ms: Option<u64>,
        now_ms: u64,
    ) -> Result<(), CacheAdmissionError> {
        self.admit_single(key, 0)?;
        self.set_value(key, CacheValue::Integer(value), ttl_ms, now_ms);
        Ok(())
    }

    /// Read-only response sizing without changing hit counters or lazy expiry.
    #[cfg(feature = "cache-server")]
    pub(crate) fn reply_value_bound(&self, key: &[u8], now_ms: u64) -> usize {
        let Some(entry) = self
            .find_slot(key, Self::hash(key))
            .and_then(|slot| self.slots[slot as usize].entry.as_ref())
        else {
            return 5;
        };
        if entry
            .expires_at_ms
            .is_some_and(|deadline| deadline <= now_ms)
        {
            return 5;
        }
        match entry.value.view(&self.arena) {
            CacheValueView::Integer(_) => 32,
            CacheValueView::Bytes(bytes) => bytes.len().saturating_add(32),
        }
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
            self.set_integer(key, delta, None, now_ms)
                .map_err(CacheIncrementError::Admission)?;
            return Ok(delta);
        };

        let current = self.slots[slot_id as usize]
            .entry
            .as_ref()
            .expect("live slot vanished")
            .value;

        let base = parse_cache_integer(current.view(&self.arena))?;

        let next = base
            .checked_add(delta)
            .ok_or(CacheIncrementError::Overflow)?;
        if let CacheValue::Bytes(bytes) = current {
            bytes.release(&mut self.arena);
        }
        self.slots[slot_id as usize]
            .entry
            .as_mut()
            .expect("live slot vanished")
            .value = CacheValue::Integer(next);
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

    /// Inspect at most max_items expiration records and return the number of
    /// live entries removed. Deferred records remain queued for later sweeps.
    pub fn purge_expired(&mut self, now_ms: u64, max_items: usize) -> usize {
        if max_items == 0 {
            return 0;
        }
        self.expiry_scratch.clear();
        self.expiry
            .drain_candidates(now_ms, &mut self.expiry_scratch, max_items);

        let mut expired = 0usize;
        let mut candidates = std::mem::take(&mut self.expiry_scratch);

        for item in candidates.iter().copied() {
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

    pub fn len(&self) -> usize {
        self.index_len
    }

    pub fn is_empty(&self) -> bool {
        self.index_len == 0
    }

    pub fn stats(&self) -> CacheStats {
        self.stats
    }

    fn retained_capacity_bytes(&self) -> usize {
        fn vec_bytes<T>(v: &Vec<T>) -> usize {
            v.capacity().saturating_mul(std::mem::size_of::<T>())
        }
        let mut bytes = std::mem::size_of::<Self>();
        for size in [
            vec_bytes(&self.arena.bytes),
            vec_bytes(&self.arena.free),
            vec_bytes(&self.slots),
            vec_bytes(&self.free_slots),
            vec_bytes(&self.index),
            vec_bytes(&self.expiry.buckets),
            vec_bytes(&self.expiry.nodes),
            vec_bytes(&self.expiry.queued),
            self.expiry
                .pending
                .capacity()
                .saturating_mul(std::mem::size_of::<usize>()),
            vec_bytes(&self.expiry_scratch),
        ] {
            bytes = bytes.saturating_add(size);
        }
        for list in &self.arena.free {
            bytes = bytes.saturating_add(vec_bytes(list));
        }
        bytes
    }

    pub fn memory_stats(&self) -> CacheMemoryStats {
        CacheMemoryStats {
            entries: self.index_len,
            index_capacity: self.index.len(),
            arena_reserved_bytes: self.arena.reserved_bytes(),
            retained_capacity_bytes: self.retained_capacity_bytes(),
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
    fn inline_values_do_not_touch_arena() {
        let mut store = CacheStore::new();
        store.set_bytes(b"k", b"small", None, 0).unwrap();
        assert_eq!(store.memory_stats().arena_reserved_bytes, 0);
        assert_eq!(store.get(b"k", 0), Some(CacheValueView::Bytes(b"small")));
    }

    #[test]
    fn large_value_blocks_are_reused_after_delete() {
        let mut store = CacheStore::new();
        let a = vec![1u8; 100];
        let b = vec![2u8; 100];
        store.set_bytes(b"a", &a, None, 0).unwrap();
        let reserved = store.memory_stats().arena_reserved_bytes;
        assert_eq!(reserved, 128);
        assert!(store.delete(b"a"));
        store.set_bytes(b"b", &b, None, 0).unwrap();
        assert_eq!(store.memory_stats().arena_reserved_bytes, reserved);
        assert_eq!(
            store.get(b"b", 0),
            Some(CacheValueView::Bytes(b.as_slice()))
        );
    }

    #[test]
    fn ttl_is_lazy_and_wheel_reaped() {
        let mut store = CacheStore::new();
        store.set_integer(b"a", 7, Some(5), 100).unwrap();
        assert_eq!(store.get(b"a", 104), Some(CacheValueView::Integer(7)));
        assert_eq!(store.get(b"a", 105), None);

        store.set_integer(b"b", 9, Some(10), 200).unwrap();
        assert_eq!(store.purge_expired(210, 100), 1);
        assert_eq!(store.get(b"b", 210), None);
    }

    #[test]
    fn sub_tick_expiry_is_reaped_after_tick_advance() {
        let mut store = CacheStore::new();
        store.set_integer(b"short", 1, Some(5), 100).unwrap();
        assert_eq!(store.purge_expired(110, 100), 1);
        assert!(store.is_empty());
    }

    #[test]
    fn delete_churn_compacts_tombstones_before_growing_index() {
        let mut store = CacheStore::new();
        for i in 0..48u64 {
            let key = i.to_le_bytes();
            store.set_integer(&key, i as i64, None, 0).unwrap();
        }
        let capacity = store.memory_stats().index_capacity;

        for i in 0..40u64 {
            let key = i.to_le_bytes();
            assert!(store.delete(&key));
        }

        for i in 100..132u64 {
            let key = i.to_le_bytes();
            store.set_integer(&key, i as i64, None, 0).unwrap();
        }

        assert_eq!(store.memory_stats().index_capacity, capacity);
    }

    #[test]
    fn expire_and_ttl_preserve_live_key_semantics() {
        let mut store = CacheStore::new();
        store.set_bytes(b"k", b"v", None, 100).unwrap();
        assert_eq!(store.ttl(b"k", 100), CacheTtl::Persistent);
        assert!(store.expire_ms(b"k", 2_500, 100));
        assert_eq!(store.ttl(b"k", 600), CacheTtl::RemainingMs(2_000));
        assert_eq!(store.ttl(b"k", 2_600), CacheTtl::Missing);
    }

    #[test]
    fn increment_promotes_bytes_and_preserves_ttl() {
        let mut store = CacheStore::new();
        store.set_bytes(b"n", b"41", Some(5_000), 100).unwrap();
        assert_eq!(store.increment(b"n", 1, 200), Ok(42));
        assert_eq!(store.get(b"n", 200), Some(CacheValueView::Integer(42)));
        assert_eq!(store.ttl(b"n", 200), CacheTtl::RemainingMs(4_900));
    }

    #[test]
    fn increment_rejects_non_integer_and_overflow() {
        let mut store = CacheStore::new();
        store.set_bytes(b"text", b"nope", None, 0).unwrap();
        assert_eq!(
            store.increment(b"text", 1, 0),
            Err(CacheIncrementError::NotInteger)
        );
        store.set_integer(b"max", i64::MAX, None, 0).unwrap();
        assert_eq!(
            store.increment(b"max", 1, 0),
            Err(CacheIncrementError::Overflow)
        );
    }

    #[test]
    fn delete_at_treats_expired_keys_as_missing() {
        let mut store = CacheStore::new();
        store.set_bytes(b"k", b"v", Some(5), 100).unwrap();
        assert!(!store.delete_at(b"k", 105));
        assert!(store.is_empty());
    }

    #[test]
    fn stale_expiry_cannot_delete_recycled_slot() {
        let mut store = CacheStore::new();
        store.set_integer(b"old", 1, Some(10), 0).unwrap();
        assert!(store.delete(b"old"));
        store.set_integer(b"new", 2, Some(1_000), 0).unwrap();
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

    #[test]
    fn expiry_sweep_bounds_candidate_buffer_and_resumes_after_clock_jump() {
        let mut store = CacheStore::new();
        for i in 0u32..1000 {
            store
                .set_bytes(&i.to_le_bytes(), b"v", Some(10), 0)
                .unwrap();
        }
        assert_eq!(store.purge_expired(20, 0), 0);
        assert_eq!(store.expiry_scratch.capacity(), 0);
        assert_eq!(store.purge_expired(20, 3), 3);
        assert!(store.expiry_scratch.capacity() <= 8);
        // The partially drained old bucket must remain eligible even after
        // the clock leaves it; no full wheel rotation is required.
        assert_eq!(store.purge_expired(40, 3), 3);
        let mut removed = 6;
        for _ in 0..334 {
            removed += store.purge_expired(40, 3);
        }
        assert_eq!(removed, 1000);
        assert!(store.is_empty());
    }

    #[test]
    fn expiry_budget_counts_stale_records_and_future_collisions() {
        let mut store = CacheStore::new();
        let rotation = DEFAULT_WHEEL_BUCKETS as u64 * DEFAULT_WHEEL_TICK_MS;
        for i in 0u32..100 {
            store
                .set_bytes(&i.to_le_bytes(), b"future", Some(rotation + 10), 0)
                .unwrap();
        }
        store.set_bytes(b"stale", b"v", Some(10), 0).unwrap();
        store.set_bytes(b"stale", b"persistent", None, 0).unwrap();
        store.set_bytes(b"due", b"v", Some(10), 0).unwrap();
        assert_eq!(store.purge_expired(10, 1), 0);
        assert!(store.expiry_scratch.capacity() <= 4);
        let mut removed = 0;
        for _ in 0..102 {
            removed += store.purge_expired(10, 1);
        }
        assert_eq!(removed, 1);
        assert_eq!(store.len(), 101);
    }

    #[test]
    fn expiry_pending_buckets_rotate_without_starving_other_due_keys() {
        let mut store = CacheStore::new();
        for i in 0u32..100 {
            store
                .set_bytes(&i.to_le_bytes(), b"v", Some(10), 0)
                .unwrap();
        }
        store.set_bytes(b"later", b"v", Some(20), 0).unwrap();
        assert_eq!(store.purge_expired(30, 1), 1);
        assert_eq!(store.purge_expired(30, 1), 1);
        // Check physical removal rather than lazy expiry through GET.
        assert!(store
            .find_slot(b"later", CacheStore::hash(b"later"))
            .is_none());
        assert_eq!(store.len(), 99);
        assert!(store.expiry.pending.len() <= DEFAULT_WHEEL_BUCKETS);
    }

    #[test]
    fn ttl_refresh_does_not_accumulate_expiration_records() {
        let mut store = CacheStore::new();
        for i in 0..10_000u64 {
            store.set_bytes(b"hot", b"v", Some(60_000 + i), 0).unwrap();
            assert!(store.expire_ms(b"hot", 120_000 + i, 0));
        }
        assert_eq!(store.expiry.nodes.iter().flatten().count(), 1);
        assert_eq!(store.ttl(b"hot", 0), CacheTtl::RemainingMs(129_999));
        store.set_bytes(b"hot", b"persistent", None, 0).unwrap();
        assert_eq!(store.expiry.nodes.iter().flatten().count(), 0);
    }

    #[test]
    fn ttl_delete_lazy_expiry_and_slot_reuse_cancel_records() {
        let mut store = CacheStore::new();
        store.set_bytes(b"a", b"v", Some(10), 0).unwrap();
        store.set_bytes(b"b", b"v", Some(10), 0).unwrap();
        store.set_bytes(b"c", b"v", Some(10), 0).unwrap();
        assert!(store.delete(b"b"));
        assert!(!store.exists(b"a", 10));
        store.set_bytes(b"d", b"v", Some(50), 10).unwrap();
        assert_eq!(store.expiry.nodes.iter().flatten().count(), 2);
        assert_eq!(store.purge_expired(10, 1), 1);
        assert!(store.exists(b"d", 10));
        assert_eq!(store.purge_expired(60, 1), 1);
        assert!(store.is_empty());
        assert_eq!(store.expiry.nodes.iter().flatten().count(), 0);
    }

    #[test]
    fn cancellation_while_bucket_is_pending_preserves_fifo_and_reused_slots() {
        let mut store = CacheStore::new();
        for key in [b"a", b"b", b"c", b"d", b"e"] {
            store.set_bytes(key, b"v", Some(10), 0).unwrap();
        }
        assert_eq!(store.purge_expired(10, 1), 1);
        // Cancel a pending head, middle, and tail without lazy-expiry reads.
        assert!(store.delete(b"b"));
        assert!(store.delete(b"d"));
        assert!(store.delete(b"e"));
        store.set_bytes(b"new", b"v", Some(30), 10).unwrap();
        assert_eq!(store.purge_expired(20, 1), 1);
        assert_eq!(store.len(), 1);
        assert!(store.exists(b"new", 39));
        assert_eq!(store.purge_expired(40, 1), 1);
        assert!(store.is_empty());
        assert_eq!(store.expiry.nodes.iter().flatten().count(), 0);
        assert!(store
            .expiry
            .buckets
            .iter()
            .all(|b| b.head.is_none() && b.tail.is_none()));
    }

    #[test]
    fn batch_arena_preflight_counts_key_and_value_blocks_and_preserves_ttl() {
        let mut store = CacheStore::with_limits(CacheLimits {
            max_entries: 4,
            max_arena_bytes: 64,
        });
        store.set_bytes(b"old", b"v", Some(100), 0).unwrap();
        let long = [b'x'; 32];
        let other = [b'y'; 32];
        assert_eq!(
            store.set_batch(&[(b"old", &long), (&long, &other)], 0),
            Err(CacheAdmissionError::ArenaLimit)
        );
        assert_eq!(store.get(b"old", 0), Some(CacheValueView::Bytes(b"v")));
        assert_eq!(store.ttl(b"old", 0), CacheTtl::RemainingMs(100));
        assert_eq!(store.memory_stats().arena_reserved_bytes, 0);
        store.set_bytes(&long, &other, None, 0).unwrap();
        let stats = store.memory_stats();
        assert_eq!(stats.arena_reserved_bytes, 64);
        assert!(stats.retained_capacity_bytes >= 64);
        assert!(store.delete(&long));
        store.set_bytes(&other, &long, None, 0).unwrap();
        assert_eq!(store.memory_stats().arena_reserved_bytes, 64);
    }
}
