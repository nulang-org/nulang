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

use std::collections::{hash_map::RandomState, HashSet, VecDeque};
use std::hash::{BuildHasher, Hasher};

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
const DEFAULT_MAX_KEY_BYTES: usize = 64 * 1024;
const DEFAULT_MAX_VALUE_BYTES: usize = 64 * 1024 * 1024;
const DEFAULT_MAX_ENTRIES: usize = 1_000_000;
const DEFAULT_MAX_ARENA_BYTES: usize = 512 * 1024 * 1024;

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

    fn additional_reserved_for(&self, lengths: &[usize]) -> usize {
        let mut claimed_free = [0usize; FREE_LIST_COUNT];
        let mut growth = 0usize;

        for &len in lengths {
            if len <= INLINE_BYTES {
                continue;
            }
            let (class, capacity) = Self::class_for(len);
            if claimed_free[class] < self.free[class].len() {
                claimed_free[class] += 1;
            } else {
                growth = growth.saturating_add(capacity);
            }
        }
        growth
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
pub enum CacheWriteError {
    KeyTooLarge,
    ValueTooLarge,
    EntryLimitReached,
    ArenaLimitReached,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheIncrementError {
    NotInteger,
    Overflow,
    WriteRejected(CacheWriteError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheEvictionPolicy {
    None,
    S3Fifo,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheConfig {
    pub max_key_bytes: usize,
    pub max_value_bytes: usize,
    pub max_entries: usize,
    pub max_arena_bytes: usize,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            max_key_bytes: DEFAULT_MAX_KEY_BYTES,
            max_value_bytes: DEFAULT_MAX_VALUE_BYTES,
            max_entries: DEFAULT_MAX_ENTRIES,
            max_arena_bytes: DEFAULT_MAX_ARENA_BYTES,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EvictionQueue {
    Small,
    Main,
}

#[derive(Debug)]
struct Entry {
    hash: u64,
    key: PackedBytes,
    value: CacheValue,
    expires_at_ms: Option<u64>,
    frequency: u8,
    eviction_queue: EvictionQueue,
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

#[derive(Debug, Clone, Copy)]
struct EvictionRef {
    slot: u32,
    generation: u32,
}

#[derive(Debug)]
struct S3Fifo {
    policy: CacheEvictionPolicy,
    small: VecDeque<EvictionRef>,
    main: VecDeque<EvictionRef>,
    ghost: VecDeque<u64>,
    ghost_set: HashSet<u64>,
    small_entries: usize,
    main_entries: usize,
    small_target: usize,
    ghost_capacity: usize,
}

impl S3Fifo {
    fn new(policy: CacheEvictionPolicy, max_entries: usize) -> Self {
        Self {
            policy,
            small: VecDeque::new(),
            main: VecDeque::new(),
            ghost: VecDeque::new(),
            ghost_set: HashSet::new(),
            small_entries: 0,
            main_entries: 0,
            small_target: (max_entries / 10).max(1),
            ghost_capacity: max_entries.max(1),
        }
    }

    fn enabled(&self) -> bool {
        self.policy == CacheEvictionPolicy::S3Fifo
    }

    fn choose_admission_queue(&mut self, hash: u64) -> EvictionQueue {
        if self.ghost_set.remove(&hash) {
            EvictionQueue::Main
        } else {
            EvictionQueue::Small
        }
    }

    fn record_insert(&mut self, queue: EvictionQueue, item: EvictionRef) {
        match queue {
            EvictionQueue::Small => {
                self.small_entries += 1;
                self.small.push_back(item);
            }
            EvictionQueue::Main => {
                self.main_entries += 1;
                self.main.push_back(item);
            }
        }
    }

    fn record_remove(&mut self, queue: EvictionQueue) {
        match queue {
            EvictionQueue::Small => self.small_entries = self.small_entries.saturating_sub(1),
            EvictionQueue::Main => self.main_entries = self.main_entries.saturating_sub(1),
        }
    }

    fn record_promotion(&mut self, item: EvictionRef) {
        self.small_entries = self.small_entries.saturating_sub(1);
        self.main_entries += 1;
        self.main.push_back(item);
    }

    fn remember_ghost(&mut self, hash: u64) {
        if self.ghost_set.insert(hash) {
            self.ghost.push_back(hash);
        }
        while self.ghost_set.len() > self.ghost_capacity {
            let Some(oldest) = self.ghost.pop_front() else {
                break;
            };
            self.ghost_set.remove(&oldest);
        }
    }
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
    pub evictions: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheMemoryStats {
    pub entries: usize,
    pub index_capacity: usize,
    pub arena_reserved_bytes: usize,
    pub reusable_slots: usize,
}

/// Shard-local compact cache storage.
///
/// The owning runtime shard keeps `CacheStore` thread-confined by runtime
/// contract and calls it directly; the type does not require synchronization
/// internally.
#[derive(Debug)]
pub struct CacheStore {
    config: CacheConfig,
    hash_builder: RandomState,
    arena: ByteArena,
    slots: Vec<EntrySlot>,
    free_slots: Vec<u32>,
    index: Vec<Bucket>,
    index_len: usize,
    tombstones: usize,
    expiry: ExpirationWheel,
    expiry_scratch: Vec<ExpirationRef>,
    eviction: S3Fifo,
    stats: CacheStats,
}

impl Default for CacheStore {
    fn default() -> Self {
        Self::new()
    }
}

impl CacheStore {
    pub fn new() -> Self {
        Self::with_config_and_eviction(CacheConfig::default(), CacheEvictionPolicy::S3Fifo)
    }

    /// Construct a cache with hard admission limits and no automatic eviction.
    ///
    /// This preserves a useful fail-closed mode for callers that need an
    /// explicit capacity error instead of cache replacement semantics.
    pub fn with_config(config: CacheConfig) -> Self {
        Self::with_config_and_eviction(config, CacheEvictionPolicy::None)
    }

    pub fn with_config_and_eviction(
        config: CacheConfig,
        eviction_policy: CacheEvictionPolicy,
    ) -> Self {
        assert!(
            config.max_key_bytes > 0,
            "cache max_key_bytes must be non-zero"
        );
        assert!(
            config.max_value_bytes > 0,
            "cache max_value_bytes must be non-zero"
        );
        assert!(config.max_entries > 0, "cache max_entries must be non-zero");
        assert!(
            config.max_arena_bytes > 0,
            "cache max_arena_bytes must be non-zero"
        );
        Self {
            config,
            hash_builder: RandomState::new(),
            arena: ByteArena::new(),
            slots: Vec::new(),
            free_slots: Vec::new(),
            index: vec![Bucket::EMPTY; DEFAULT_INDEX_CAPACITY],
            index_len: 0,
            tombstones: 0,
            expiry: ExpirationWheel::new(DEFAULT_WHEEL_BUCKETS, DEFAULT_WHEEL_TICK_MS),
            expiry_scratch: Vec::new(),
            eviction: S3Fifo::new(eviction_policy, config.max_entries),
            stats: CacheStats::default(),
        }
    }

    pub fn config(&self) -> CacheConfig {
        self.config
    }

    #[inline]
    fn hash(&self, key: &[u8]) -> u64 {
        let mut hasher = self.hash_builder.build_hasher();
        hasher.write(key);
        hasher.finish()
    }

    fn validate_bytes_write(&self, key: &[u8], value: &[u8]) -> Result<(), CacheWriteError> {
        if key.len() > self.config.max_key_bytes {
            return Err(CacheWriteError::KeyTooLarge);
        }
        if value.len() > self.config.max_value_bytes {
            return Err(CacheWriteError::ValueTooLarge);
        }

        let hash = self.hash(key);
        let existing = self.find_slot(key, hash).is_some();
        if !existing && self.index_len >= self.config.max_entries {
            return Err(CacheWriteError::EntryLimitReached);
        }

        let lengths = if existing {
            [value.len(), 0]
        } else {
            [key.len(), value.len()]
        };
        let additional = self.arena.additional_reserved_for(&lengths);
        if self.arena.reserved_bytes().saturating_add(additional) > self.config.max_arena_bytes {
            return Err(CacheWriteError::ArenaLimitReached);
        }
        Ok(())
    }

    fn validate_bytes_batch(&self, pairs: &[(&[u8], &[u8])]) -> Result<(), CacheWriteError> {
        let mut new_entries = 0usize;
        let mut arena_lengths = Vec::with_capacity(pairs.len().saturating_mul(2));

        for (index, &(key, value)) in pairs.iter().enumerate() {
            if key.len() > self.config.max_key_bytes {
                return Err(CacheWriteError::KeyTooLarge);
            }
            if value.len() > self.config.max_value_bytes {
                return Err(CacheWriteError::ValueTooLarge);
            }

            let hash = self.hash(key);
            let exists_in_store = self.find_slot(key, hash).is_some();
            let exists_earlier_in_batch = pairs[..index]
                .iter()
                .any(|(previous_key, _)| *previous_key == key);
            if !exists_in_store && !exists_earlier_in_batch {
                new_entries = new_entries.saturating_add(1);
                arena_lengths.push(key.len());
            }
            // Conservatively account for every value in the batch. Sequential
            // replacements may release an older block earlier, so this is an
            // upper bound on peak arena growth rather than an underestimate.
            arena_lengths.push(value.len());
        }

        if self.index_len.saturating_add(new_entries) > self.config.max_entries {
            return Err(CacheWriteError::EntryLimitReached);
        }
        let additional = self.arena.additional_reserved_for(&arena_lengths);
        if self.arena.reserved_bytes().saturating_add(additional) > self.config.max_arena_bytes {
            return Err(CacheWriteError::ArenaLimitReached);
        }
        Ok(())
    }

    fn prepare_bytes_write(&mut self, key: &[u8], value: &[u8]) -> Result<(), CacheWriteError> {
        if key.len() > self.config.max_key_bytes {
            return Err(CacheWriteError::KeyTooLarge);
        }
        if value.len() > self.config.max_value_bytes {
            return Err(CacheWriteError::ValueTooLarge);
        }

        loop {
            match self.validate_bytes_write(key, value) {
                Ok(()) => return Ok(()),
                Err(error @ CacheWriteError::EntryLimitReached)
                | Err(error @ CacheWriteError::ArenaLimitReached) => {
                    if !self.eviction.enabled() || !self.evict_one() {
                        return Err(error);
                    }
                }
                Err(error) => return Err(error),
            }
        }
    }

    fn validate_integer_write(&self, key: &[u8]) -> Result<(), CacheWriteError> {
        if key.len() > self.config.max_key_bytes {
            return Err(CacheWriteError::KeyTooLarge);
        }
        let hash = self.hash(key);
        if self.find_slot(key, hash).is_some() {
            return Ok(());
        }
        if self.index_len >= self.config.max_entries {
            return Err(CacheWriteError::EntryLimitReached);
        }
        let additional = self.arena.additional_reserved_for(&[key.len()]);
        if self.arena.reserved_bytes().saturating_add(additional) > self.config.max_arena_bytes {
            return Err(CacheWriteError::ArenaLimitReached);
        }
        Ok(())
    }

    fn prepare_integer_write(&mut self, key: &[u8]) -> Result<(), CacheWriteError> {
        if key.len() > self.config.max_key_bytes {
            return Err(CacheWriteError::KeyTooLarge);
        }
        loop {
            match self.validate_integer_write(key) {
                Ok(()) => return Ok(()),
                Err(error @ CacheWriteError::EntryLimitReached)
                | Err(error @ CacheWriteError::ArenaLimitReached) => {
                    if !self.eviction.enabled() || !self.evict_one() {
                        return Err(error);
                    }
                }
                Err(error) => return Err(error),
            }
        }
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
        let hash = self.hash(key);
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

    fn record_hit(&mut self, slot_id: u32) {
        if let Some(entry) = self
            .slots
            .get_mut(slot_id as usize)
            .and_then(|slot| slot.entry.as_mut())
        {
            entry.frequency = entry.frequency.saturating_add(1).min(3);
        }
    }

    fn eviction_ref_is_live(&self, item: EvictionRef, queue: EvictionQueue) -> bool {
        self.slots
            .get(item.slot as usize)
            .is_some_and(|slot| {
                slot.generation == item.generation
                    && slot
                        .entry
                        .as_ref()
                        .is_some_and(|entry| entry.eviction_queue == queue)
            })
    }

    fn evict_from_small(&mut self) -> bool {
        let attempts = self.eviction.small.len().saturating_add(1);
        for _ in 0..attempts {
            let Some(item) = self.eviction.small.pop_front() else {
                return false;
            };
            if !self.eviction_ref_is_live(item, EvictionQueue::Small) {
                continue;
            }

            let promote = self.slots[item.slot as usize]
                .entry
                .as_ref()
                .is_some_and(|entry| entry.frequency > 1);
            if promote {
                let entry = self.slots[item.slot as usize]
                    .entry
                    .as_mut()
                    .expect("validated live eviction entry");
                entry.frequency = 0;
                entry.eviction_queue = EvictionQueue::Main;
                self.eviction.record_promotion(item);
                continue;
            }

            let hash = self.slots[item.slot as usize]
                .entry
                .as_ref()
                .expect("validated live eviction entry")
                .hash;
            if self.remove_slot(item.slot) {
                self.eviction.remember_ghost(hash);
                self.stats.evictions += 1;
                return true;
            }
        }
        false
    }

    fn evict_from_main(&mut self) -> bool {
        // Frequency is capped at three, so four full passes are sufficient to
        // age every live main-queue entry to an evictable state.
        let attempts = self.eviction.main.len().saturating_mul(4).saturating_add(1);
        for _ in 0..attempts {
            let Some(item) = self.eviction.main.pop_front() else {
                return false;
            };
            if !self.eviction_ref_is_live(item, EvictionQueue::Main) {
                continue;
            }

            let frequency = self.slots[item.slot as usize]
                .entry
                .as_ref()
                .expect("validated live eviction entry")
                .frequency;
            if frequency > 0 {
                self.slots[item.slot as usize]
                    .entry
                    .as_mut()
                    .expect("validated live eviction entry")
                    .frequency = frequency - 1;
                self.eviction.main.push_back(item);
                continue;
            }

            if self.remove_slot(item.slot) {
                self.stats.evictions += 1;
                return true;
            }
        }
        false
    }

    fn evict_one(&mut self) -> bool {
        if !self.eviction.enabled() || self.index_len == 0 {
            return false;
        }

        let evicted = if self.eviction.small_entries > self.eviction.small_target {
            self.evict_from_small() || self.evict_from_main()
        } else {
            self.evict_from_main() || self.evict_from_small()
        };
        self.maybe_compact_eviction_queues();
        evicted
    }

    fn maybe_compact_eviction_queues(&mut self) {
        let queued = self.eviction.small.len().saturating_add(self.eviction.main.len());
        let threshold = self.index_len.max(64).saturating_mul(4).saturating_add(64);
        if queued <= threshold {
            return;
        }

        self.eviction.small.clear();
        self.eviction.main.clear();
        self.eviction.small_entries = 0;
        self.eviction.main_entries = 0;
        for (slot_id, slot) in self.slots.iter().enumerate() {
            let Some(entry) = slot.entry.as_ref() else {
                continue;
            };
            let item = EvictionRef {
                slot: slot_id as u32,
                generation: slot.generation,
            };
            self.eviction.record_insert(entry.eviction_queue, item);
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
        self.eviction.record_remove(entry.eviction_queue);
        entry.key.release(&mut self.arena);
        entry.value.release(&mut self.arena);
        self.free_slots.push(slot_id);
        true
    }

    fn set_value(&mut self, key: &[u8], value: CacheValue, ttl_ms: Option<u64>, now_ms: u64) {
        let hash = self.hash(key);
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
            entry.frequency = entry.frequency.saturating_add(1).min(3);
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
        let eviction_queue = self.eviction.choose_admission_queue(hash);
        let entry = Entry {
            hash,
            key: packed_key,
            value,
            expires_at_ms,
            frequency: u8::from(eviction_queue == EvictionQueue::Main),
            eviction_queue,
        };
        let (slot_id, generation) = self.allocate_slot(entry);
        self.insert_bucket_raw(hash, slot_id);
        self.index_len += 1;
        self.eviction.record_insert(
            eviction_queue,
            EvictionRef {
                slot: slot_id,
                generation,
            },
        );
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

    fn set_bytes_unchecked(&mut self, key: &[u8], value: &[u8], ttl_ms: Option<u64>, now_ms: u64) {
        let value = CacheValue::Bytes(PackedBytes::pack(value, &mut self.arena));
        self.set_value(key, value, ttl_ms, now_ms);
    }

    pub fn try_set_bytes(
        &mut self,
        key: &[u8],
        value: &[u8],
        ttl_ms: Option<u64>,
        now_ms: u64,
    ) -> Result<(), CacheWriteError> {
        self.prepare_bytes_write(key, value)?;
        self.set_bytes_unchecked(key, value, ttl_ms, now_ms);
        Ok(())
    }

    pub fn try_set_many_bytes(
        &mut self,
        pairs: &[(&[u8], &[u8])],
        ttl_ms: Option<u64>,
        now_ms: u64,
    ) -> Result<(), CacheWriteError> {
        self.validate_bytes_batch(pairs)?;
        for &(key, value) in pairs {
            self.set_bytes_unchecked(key, value, ttl_ms, now_ms);
        }
        Ok(())
    }

    pub fn set_bytes(&mut self, key: &[u8], value: &[u8], ttl_ms: Option<u64>, now_ms: u64) {
        self.try_set_bytes(key, value, ttl_ms, now_ms)
            .expect("trusted cache write exceeds configured limits");
    }

    pub fn try_set_integer(
        &mut self,
        key: &[u8],
        value: i64,
        ttl_ms: Option<u64>,
        now_ms: u64,
    ) -> Result<(), CacheWriteError> {
        self.prepare_integer_write(key)?;
        self.set_value(key, CacheValue::Integer(value), ttl_ms, now_ms);
        Ok(())
    }

    pub fn set_integer(&mut self, key: &[u8], value: i64, ttl_ms: Option<u64>, now_ms: u64) {
        self.try_set_integer(key, value, ttl_ms, now_ms)
            .expect("trusted cache write exceeds configured limits");
    }

    pub fn get(&mut self, key: &[u8], now_ms: u64) -> Option<CacheValueView<'_>> {
        let Some(slot_id) = self.live_slot_id(key, now_ms) else {
            self.stats.misses += 1;
            return None;
        };

        self.stats.hits += 1;
        self.record_hit(slot_id);
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
            self.try_set_integer(key, delta, None, now_ms)
                .map_err(CacheIncrementError::WriteRejected)?;
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
        self.record_hit(slot_id);
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
        let hash = self.hash(key);
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

    #[test]
    fn admission_limits_reject_oversized_network_writes_without_mutation() {
        let mut store = CacheStore::with_config(CacheConfig {
            max_key_bytes: 4,
            max_value_bytes: 8,
            max_entries: 2,
            max_arena_bytes: 64,
        });

        assert_eq!(
            store.try_set_bytes(b"toolong", b"v", None, 0),
            Err(CacheWriteError::KeyTooLarge)
        );
        assert_eq!(
            store.try_set_bytes(b"k", b"123456789", None, 0),
            Err(CacheWriteError::ValueTooLarge)
        );
        assert!(store.is_empty());
    }

    #[test]
    fn entry_limit_is_enforced_before_index_growth() {
        let mut store = CacheStore::with_config(CacheConfig {
            max_key_bytes: 64,
            max_value_bytes: 64,
            max_entries: 1,
            max_arena_bytes: 1024,
        });
        assert_eq!(store.try_set_bytes(b"a", b"1", None, 0), Ok(()));
        assert_eq!(
            store.try_set_bytes(b"b", b"2", None, 0),
            Err(CacheWriteError::EntryLimitReached)
        );
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn mset_style_batch_rejects_before_partial_mutation() {
        let mut store = CacheStore::with_config(CacheConfig {
            max_key_bytes: 64,
            max_value_bytes: 64,
            max_entries: 1,
            max_arena_bytes: 1024,
        });
        let pairs = [
            (b"a".as_slice(), b"1".as_slice()),
            (b"b".as_slice(), b"2".as_slice()),
        ];
        assert_eq!(
            store.try_set_many_bytes(&pairs, None, 0),
            Err(CacheWriteError::EntryLimitReached)
        );
        assert!(store.is_empty());
    }

    #[test]
    fn s3_fifo_evicts_cold_entry_and_retains_reused_entry() {
        let mut store = CacheStore::with_config_and_eviction(
            CacheConfig {
                max_key_bytes: 64,
                max_value_bytes: 64,
                max_entries: 2,
                max_arena_bytes: 1024,
            },
            CacheEvictionPolicy::S3Fifo,
        );

        store.try_set_bytes(b"a", b"1", None, 0).unwrap();
        store.try_set_bytes(b"b", b"2", None, 0).unwrap();
        assert_eq!(store.get(b"a", 0), Some(CacheValueView::Bytes(b"1")));
        assert_eq!(store.get(b"a", 0), Some(CacheValueView::Bytes(b"1")));

        store.try_set_bytes(b"c", b"3", None, 0).unwrap();

        assert_eq!(store.get(b"a", 0), Some(CacheValueView::Bytes(b"1")));
        assert_eq!(store.get(b"b", 0), None);
        assert_eq!(store.get(b"c", 0), Some(CacheValueView::Bytes(b"3")));
        assert_eq!(store.stats().evictions, 1);
    }

    #[test]
    fn s3_fifo_ghost_hit_admits_directly_to_main() {
        let mut store = CacheStore::with_config_and_eviction(
            CacheConfig {
                max_key_bytes: 64,
                max_value_bytes: 64,
                max_entries: 2,
                max_arena_bytes: 1024,
            },
            CacheEvictionPolicy::S3Fifo,
        );

        store.set_bytes(b"a", b"1", None, 0);
        store.set_bytes(b"b", b"2", None, 0);
        store.set_bytes(b"c", b"3", None, 0);
        assert_eq!(store.get(b"a", 0), None);

        store.set_bytes(b"a", b"4", None, 0);
        let hash = store.hash(b"a");
        let slot_id = store.find_slot(b"a", hash).expect("re-admitted key");
        assert_eq!(
            store.slots[slot_id as usize]
                .entry
                .as_ref()
                .expect("live entry")
                .eviction_queue,
            EvictionQueue::Main
        );
    }

    #[test]
    fn arena_limit_accounts_for_large_value_growth() {
        let mut store = CacheStore::with_config(CacheConfig {
            max_key_bytes: 64,
            max_value_bytes: 1024,
            max_entries: 8,
            max_arena_bytes: 64,
        });
        assert_eq!(store.try_set_bytes(b"k", &[7; 40], None, 0), Ok(()));
        assert_eq!(store.memory_stats().arena_reserved_bytes, 64);
        assert_eq!(
            store.try_set_bytes(b"k2", &[8; 40], None, 0),
            Err(CacheWriteError::ArenaLimitReached)
        );
    }
}
