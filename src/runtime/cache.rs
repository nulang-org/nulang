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
                out.extend(std::mem::take(bucket));
            }
        } else {
            // Include the current bucket even when no full tick elapsed so
            // sub-tick TTLs can be reaped by an explicit purge call.
            let start = if elapsed == 0 { current } else { last + 1 };
            for tick in start..=current {
                let idx = (tick % bucket_count) as usize;
                out.extend(std::mem::take(&mut self.buckets[idx]));
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

/// Shard-local compact cache storage.
///
/// `CacheStore` is intentionally `!Sync` by usage rather than by marker:
/// the owning runtime shard keeps it thread-confined and calls methods directly.
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

    fn ensure_index_capacity(&mut self) {
        let used = self.index_len + self.tombstones + 1;
        if used * 10 >= self.index.len() * 7 {
            self.rehash(self.index.len() * 2);
        } else if self.tombstones > self.index_len && self.tombstones > 32 {
            self.rehash(self.index.len());
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

    fn set_value(
        &mut self,
        key: &[u8],
        value: CacheValue,
        ttl_ms: Option<u64>,
        now_ms: u64,
    ) {
        let hash = Self::hash(key);
        let expires_at_ms = ttl_ms.map(|ttl| now_ms.saturating_add(ttl));

        if let Some(slot_id) = self.find_slot(key, hash) {
            let slot = &mut self.slots[slot_id as usize];
            let entry = slot.entry.as_mut().expect("live index points to live entry");
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
        let hash = Self::hash(key);
        let Some(slot_id) = self.find_slot(key, hash) else {
            self.stats.misses += 1;
            return None;
        };

        let expired = self.slots[slot_id as usize]
            .entry
            .as_ref()
            .and_then(|entry| entry.expires_at_ms)
            .is_some_and(|deadline| deadline <= now_ms);

        if expired {
            self.remove_slot(slot_id);
            self.stats.expirations += 1;
            self.stats.misses += 1;
            return None;
        }

        self.stats.hits += 1;
        let entry = self.slots[slot_id as usize]
            .entry
            .as_ref()
            .expect("live slot vanished");
        Some(entry.value.view(&self.arena))
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
        let mut deferred = Vec::new();
        let candidates = std::mem::take(&mut self.expiry_scratch);

        for item in candidates {
            if expired >= max_items {
                deferred.push(item);
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

        for item in deferred {
            self.expiry.schedule(item, now_ms);
        }

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
        assert_eq!(store.get(b"b", 0), Some(CacheValueView::Bytes(b.as_slice())));
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
        assert_eq!(redis_slot(b"user:{42}:profile"), redis_slot(b"user:{42}:sessions"));
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
