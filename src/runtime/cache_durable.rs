//! Single-owner native durable cache. Mutation records contain after-images
//! with absolute Unix deadlines, never process-relative timestamps.
//! Optional durable reactors use this owner; memory mode bypasses it.

use super::cache::{
    parse_cache_integer, CacheAdmissionError, CacheIncrementError, CacheLimits, CacheStore,
    CacheTtl, CacheValueView,
};
use super::cache_wal::{CacheWal, CheckpointReport, ClockFloor, WalLimits, WalSync};
use std::io;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheTime {
    pub monotonic_ms: u64,
    pub unix_ms: u64,
}

#[derive(Debug)]
pub enum DurableError {
    Admission(CacheAdmissionError),
    Increment(CacheIncrementError),
    Journal(io::Error),
    Poisoned,
    InvalidTime,
}

impl From<io::Error> for DurableError {
    fn from(error: io::Error) -> Self {
        Self::Journal(error)
    }
}
impl From<CacheAdmissionError> for DurableError {
    fn from(error: CacheAdmissionError) -> Self {
        Self::Admission(error)
    }
}
impl From<CacheIncrementError> for DurableError {
    fn from(error: CacheIncrementError) -> Self {
        Self::Increment(error)
    }
}
impl std::fmt::Display for DurableError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for DurableError {}

#[derive(Debug)]
enum Mutation<'a> {
    Bytes(&'a [u8], &'a [u8], Option<u64>),
    Integer(&'a [u8], i64, Option<u64>),
    Batch(Vec<(&'a [u8], &'a [u8])>),
    Delete(Vec<&'a [u8]>),
}

const PAYLOAD_VERSION: u8 = 1;
const MAX_BATCH_ITEMS: usize = 65_536;

fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid cache mutation payload")
}
fn too_large() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "cache mutation exceeds journal bounds",
    )
}

struct Encoder {
    bytes: Vec<u8>,
    limit: usize,
}
impl Encoder {
    fn add(&mut self, bytes: &[u8]) -> io::Result<()> {
        if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
            return Err(too_large());
        }
        self.bytes
            .try_reserve(bytes.len())
            .map_err(io::Error::other)?;
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }
    fn count(&mut self, n: usize) -> io::Result<()> {
        self.add(&u32::try_from(n).map_err(|_| too_large())?.to_le_bytes())
    }
    fn blob(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.count(bytes.len())?;
        self.add(bytes)
    }
    fn deadline(&mut self, deadline: Option<u64>) -> io::Result<()> {
        match deadline {
            None => self.add(&[0]),
            Some(value) => {
                self.add(&[1])?;
                self.add(&value.to_le_bytes())
            }
        }
    }
}

struct Decoder<'a>(&'a [u8]);
impl<'a> Decoder<'a> {
    fn take(&mut self, len: usize) -> io::Result<&'a [u8]> {
        let (head, tail) = self.0.split_at_checked(len).ok_or_else(invalid)?;
        self.0 = tail;
        Ok(head)
    }
    fn byte(&mut self) -> io::Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn count(&mut self) -> io::Result<usize> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()) as usize)
    }
    fn blob(&mut self) -> io::Result<&'a [u8]> {
        let len = self.count()?;
        self.take(len)
    }
    fn deadline(&mut self) -> io::Result<Option<u64>> {
        match self.byte()? {
            0 => Ok(None),
            1 => Ok(Some(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))),
            _ => Err(invalid()),
        }
    }
    fn bounded_count(&mut self, min_bytes: usize) -> io::Result<usize> {
        let n = self.count()?;
        if n > MAX_BATCH_ITEMS || n > self.0.len() / min_bytes {
            return Err(invalid());
        }
        Ok(n)
    }
}

fn encode(record: &Mutation<'_>, limit: usize) -> io::Result<Vec<u8>> {
    let mut out = Encoder {
        bytes: Vec::new(),
        limit,
    };
    out.add(&[PAYLOAD_VERSION])?;
    match record {
        Mutation::Bytes(key, value, deadline) => {
            out.add(&[0])?;
            out.blob(key)?;
            out.blob(value)?;
            out.deadline(*deadline)?;
        }
        Mutation::Integer(key, value, deadline) => {
            out.add(&[1])?;
            out.blob(key)?;
            out.add(&value.to_le_bytes())?;
            out.deadline(*deadline)?;
        }
        Mutation::Batch(pairs) => {
            if pairs.len() > MAX_BATCH_ITEMS {
                return Err(too_large());
            }
            out.add(&[2])?;
            out.count(pairs.len())?;
            for (key, value) in pairs {
                out.blob(key)?;
                out.blob(value)?;
            }
        }
        Mutation::Delete(keys) => {
            if keys.len() > MAX_BATCH_ITEMS {
                return Err(too_large());
            }
            out.add(&[3])?;
            out.count(keys.len())?;
            for key in keys {
                out.blob(key)?;
            }
        }
    }
    Ok(out.bytes)
}

fn decode(payload: &[u8]) -> io::Result<Mutation<'_>> {
    let mut d = Decoder(payload);
    if d.byte()? != PAYLOAD_VERSION {
        return Err(invalid());
    }
    let record = match d.byte()? {
        0 => Mutation::Bytes(d.blob()?, d.blob()?, d.deadline()?),
        1 => Mutation::Integer(
            d.blob()?,
            i64::from_le_bytes(d.take(8)?.try_into().unwrap()),
            d.deadline()?,
        ),
        2 => {
            let n = d.bounded_count(8)?;
            let mut pairs = Vec::new();
            pairs.try_reserve_exact(n).map_err(io::Error::other)?;
            for _ in 0..n {
                pairs.push((d.blob()?, d.blob()?));
            }
            Mutation::Batch(pairs)
        }
        3 => {
            let n = d.bounded_count(4)?;
            let mut keys = Vec::new();
            keys.try_reserve_exact(n).map_err(io::Error::other)?;
            for _ in 0..n {
                keys.push(d.blob()?);
            }
            Mutation::Delete(keys)
        }
        _ => return Err(invalid()),
    };
    if !d.0.is_empty() {
        return Err(invalid());
    }
    Ok(record)
}

fn absolute(ttl: Option<u64>, time: CacheTime) -> Result<Option<u64>, DurableError> {
    ttl.map(|ms| {
        time.monotonic_ms
            .checked_add(ms)
            .ok_or(DurableError::InvalidTime)?;
        time.unix_ms
            .checked_add(ms)
            .ok_or(DurableError::InvalidTime)
    })
    .transpose()
}

fn remaining(deadline: Option<u64>, time: CacheTime) -> Result<Option<u64>, DurableError> {
    let ttl = deadline.map(|d| d.saturating_sub(time.unix_ms));
    if let Some(ms) = ttl {
        time.monotonic_ms
            .checked_add(ms)
            .ok_or(DurableError::InvalidTime)?;
    }
    Ok(ttl)
}

impl Mutation<'_> {
    fn preflight(&self, store: &CacheStore, time: CacheTime) -> Result<(), DurableError> {
        match self {
            Self::Bytes(key, value, deadline) => {
                if remaining(*deadline, time)? != Some(0) {
                    store.admit_single(key, value.len())?;
                }
            }
            Self::Integer(key, _, deadline) => {
                if remaining(*deadline, time)? != Some(0) {
                    store.admit_single(key, 0)?;
                }
            }
            Self::Batch(pairs) => store.admit_batch(pairs)?,
            Self::Delete(_) => {}
        }
        Ok(())
    }

    fn apply(&self, store: &mut CacheStore, time: CacheTime) -> Result<usize, DurableError> {
        let now = time.monotonic_ms;
        match self {
            Self::Bytes(key, value, deadline) => {
                let ttl = remaining(*deadline, time)?;
                if ttl == Some(0) {
                    store.delete(key);
                } else {
                    store.set_bytes(key, value, ttl, now)?;
                }
            }
            Self::Integer(key, value, deadline) => {
                let ttl = remaining(*deadline, time)?;
                if ttl == Some(0) {
                    store.delete(key);
                } else {
                    store.set_integer(key, *value, ttl, now)?;
                }
            }
            Self::Batch(pairs) => store.set_batch(pairs, now)?,
            Self::Delete(keys) => {
                return Ok(keys.iter().filter(|key| store.delete_at(key, now)).count());
            }
        }
        Ok(0)
    }
}

/// Owns the store and journal together. No mutable store escape hatch.
/// Callers supply paired monotonic and wall-clock readings for each command.
pub struct DurableCache {
    store: CacheStore,
    wal: CacheWal,
    clock: ClockFloor,
    anchor: CacheTime,
    limits: WalLimits,
    sync: WalSync,
    #[cfg(feature = "cache-server")]
    group_start: Option<u64>,
    poisoned: bool,
}

impl DurableCache {
    pub fn open(
        path: impl AsRef<Path>,
        cache_limits: CacheLimits,
        limits: WalLimits,
        sync: WalSync,
        time: CacheTime,
    ) -> Result<Self, DurableError> {
        let mut store = CacheStore::with_limits(cache_limits);
        let mut clock = None;
        let wal = CacheWal::open_prepared(
            path,
            limits,
            |path, lock| {
                clock = Some(ClockFloor::open(path, lock, time.unix_ms)?);
                Ok(())
            },
            |_, bytes| {
                let record = decode(bytes)?;
                record.apply(&mut store, time).map_err(io::Error::other)?;
                Ok(())
            },
        )?;
        let clock = clock.ok_or_else(|| io::Error::other("missing cache clock floor"))?;
        Ok(Self {
            store,
            wal,
            clock,
            anchor: time,
            limits,
            sync,
            #[cfg(feature = "cache-server")]
            group_start: None,
            poisoned: false,
        })
    }
    /// Opt-in future floor reservation. Existing durable floors never shrink;
    /// reopening before the persisted reservation ends fails closed.
    pub fn configure_clock_reservation(&mut self, millis: u64) -> Result<(), DurableError> {
        self.healthy()?;
        self.clock.configure_reservation(millis)?;
        Ok(())
    }

    fn anchored_unix(&self, now_ms: u64) -> Result<u64, DurableError> {
        let elapsed = now_ms
            .checked_sub(self.anchor.monotonic_ms)
            .ok_or(DurableError::InvalidTime)?;
        self.anchor
            .unix_ms
            .checked_add(elapsed)
            .ok_or(DurableError::InvalidTime)
    }

    fn validate_time(&self, time: CacheTime) -> Result<(), DurableError> {
        if self.anchored_unix(time.monotonic_ms)? != time.unix_ms {
            return Err(DurableError::InvalidTime);
        }
        Ok(())
    }

    fn observe(&mut self, now_ms: u64) -> Result<(), DurableError> {
        let unix_ms = self.anchored_unix(now_ms)?;
        // Fail closed on any clock persistence error, including uncertainty
        // about whether the new floor was durably published.
        self.poisoned = true;
        self.clock.advance(unix_ms)?;
        self.poisoned = false;
        Ok(())
    }

    fn observe_key(&mut self, key: &[u8], now_ms: u64) -> Result<(), DurableError> {
        if self.store.has_deadline(key) {
            self.observe(now_ms)?;
        }
        Ok(())
    }

    /// Publish a complete snapshot as the new journal prefix, then continue
    /// appending to it. One entry is encoded at a time; no whole-cache clone.
    pub fn checkpoint(&mut self, time: CacheTime) -> Result<CheckpointReport, DurableError> {
        self.healthy()?;
        self.validate_time(time)?;
        if self
            .store
            .checkpoint_entries(time.monotonic_ms)
            .any(|(_, _, ttl)| ttl.is_some())
        {
            self.observe(time.monotonic_ms)?;
        }
        let entries = self.store.checkpoint_entries(time.monotonic_ms).count() as u64;
        let store = &self.store;
        let max_record = self.limits.max_record_bytes;
        let report = self.wal.rotate(entries, |replacement| {
            for (key, value, ttl) in store.checkpoint_entries(time.monotonic_ms) {
                let deadline = absolute(ttl, time).map_err(io::Error::other)?;
                let record = match value {
                    CacheValueView::Bytes(bytes) => Mutation::Bytes(key, bytes, deadline),
                    CacheValueView::Integer(value) => Mutation::Integer(key, value, deadline),
                };
                let payload = encode(&record, max_record)?;
                replacement.append(&payload, WalSync::Buffered)?;
            }
            Ok(())
        })?;
        Ok(report)
    }

    #[cfg(feature = "cache-server")]
    pub(super) fn operational_store(&self) -> &CacheStore {
        &self.store
    }

    /// Retained on-disk journal length and configured limit.
    pub fn journal_usage(&self) -> (u64, u64) {
        (self.wal.journal_bytes(), self.limits.max_file_bytes)
    }

    /// Size of the compacted live-entry snapshot, excluding expired entries.
    /// No value is copied and no file is created during this preflight.
    pub fn estimated_checkpoint_bytes(&self, now_ms: u64) -> u64 {
        self.store.checkpoint_entries(now_ms).fold(
            CacheWal::checkpoint_header_bytes(),
            |total, (key, value, ttl)| {
                // Version, opcode, key length, key, value and TTL presence.
                let value_bytes = match value {
                    CacheValueView::Bytes(bytes) => 4u64.saturating_add(bytes.len() as u64),
                    CacheValueView::Integer(_) => 8,
                };
                let payload = 7u64
                    .saturating_add(key.len() as u64)
                    .saturating_add(value_bytes)
                    .saturating_add(if ttl.is_some() { 8 } else { 0 });
                total.saturating_add(CacheWal::framed_record_bytes(payload))
            },
        )
    }

    /// Flush acknowledged buffered journal writes before orderly shutdown.
    pub fn synchronize(&mut self) -> Result<(), DurableError> {
        self.healthy()?;
        self.wal.synchronize()?;
        Ok(())
    }

    /// Record the orderly shutdown time as well as syncing mutation data.
    #[cfg(feature = "cache-server")]
    pub(super) fn synchronize_at(&mut self, now_ms: u64) -> Result<(), DurableError> {
        self.healthy()?;
        self.observe(now_ms)?;
        self.synchronize()
    }

    pub fn is_poisoned(&self) -> bool {
        self.poisoned || self.wal.is_poisoned()
    }
    pub fn last_sequence(&self) -> u64 {
        self.wal.last_sequence()
    }
    fn healthy(&self) -> Result<(), DurableError> {
        if self.is_poisoned() {
            Err(DurableError::Poisoned)
        } else {
            Ok(())
        }
    }
    // Only the owning network reactor may use this scope. No replies,
    // checkpoints, other connections, or external state access until finish.
    #[cfg(feature = "cache-server")]
    pub(super) fn supports_group_commit(&self) -> bool {
        self.sync == WalSync::Data
    }

    #[cfg(feature = "cache-server")]
    pub(super) fn group_open(&self) -> bool {
        self.group_start.is_some()
    }

    #[cfg(feature = "cache-server")]
    pub(super) fn begin_group(&mut self) -> Result<(), DurableError> {
        self.healthy()?;
        if !self.supports_group_commit() || self.group_open() {
            self.poisoned = true;
            return Err(DurableError::Poisoned);
        }
        self.group_start = Some(self.wal.last_sequence());
        Ok(())
    }

    #[cfg(feature = "cache-server")]
    pub(super) fn finish_group(&mut self) -> Result<u64, DurableError> {
        self.healthy()?;
        let start = self.group_start.ok_or(DurableError::Poisoned)?;
        let records = self.wal.last_sequence() - start;
        if records != 0 {
            self.wal.synchronize()?;
        }
        self.group_start = None;
        Ok(records)
    }

    #[cfg(all(test, feature = "cache-server"))]
    pub(super) fn fail_next_group_sync(&mut self) {
        self.wal.fail_next_sync = true;
    }

    fn commit(&mut self, payload: &[u8], time: CacheTime) -> Result<usize, DurableError> {
        self.commit_with(payload, time, |wal, payload, sync| {
            wal.append(payload, sync)
        })
    }
    fn commit_with(
        &mut self,
        payload: &[u8],
        time: CacheTime,
        append: impl FnOnce(&mut CacheWal, &[u8], WalSync) -> io::Result<u64>,
    ) -> Result<usize, DurableError> {
        self.healthy()?;
        self.validate_time(time)?;
        let record = decode(payload)?;
        record.preflight(&self.store, time)?;
        // Poison before the append so unwinding cannot expose uncertain state.
        self.poisoned = true;
        #[cfg(feature = "cache-server")]
        let sync = if self.group_open() {
            WalSync::Buffered
        } else {
            self.sync
        };
        #[cfg(not(feature = "cache-server"))]
        let sync = self.sync;
        if let Err(error) = append(&mut self.wal, payload, sync) {
            self.poisoned = self.wal.is_poisoned();
            return Err(error.into());
        }
        let result = record.apply(&mut self.store, time)?;
        self.poisoned = false;
        Ok(result)
    }
    pub fn set_bytes(
        &mut self,
        key: &[u8],
        value: &[u8],
        ttl: Option<u64>,
        time: CacheTime,
    ) -> Result<(), DurableError> {
        self.healthy()?;
        let payload = encode(
            &Mutation::Bytes(key, value, absolute(ttl, time)?),
            self.limits.max_record_bytes,
        )?;
        self.validate_time(time)?;
        if ttl.is_some() {
            self.observe(time.monotonic_ms)?;
        }
        self.commit(&payload, time)?;
        Ok(())
    }
    pub fn set_batch(
        &mut self,
        pairs: &[(&[u8], &[u8])],
        time: CacheTime,
    ) -> Result<(), DurableError> {
        self.healthy()?;
        if pairs.len() > MAX_BATCH_ITEMS {
            return Err(too_large().into());
        }
        let payload = encode(
            &Mutation::Batch(pairs.to_vec()),
            self.limits.max_record_bytes,
        )?;
        self.commit(&payload, time)?;
        Ok(())
    }
    pub fn delete_many(&mut self, keys: &[&[u8]], time: CacheTime) -> Result<usize, DurableError> {
        self.healthy()?;
        if keys.len() > MAX_BATCH_ITEMS {
            return Err(too_large().into());
        }
        self.validate_time(time)?;
        if keys.iter().any(|key| self.store.has_deadline(key)) {
            self.observe(time.monotonic_ms)?;
        }
        let payload = encode(
            &Mutation::Delete(keys.to_vec()),
            self.limits.max_record_bytes,
        )?;
        self.commit(&payload, time)
    }
    pub fn increment(
        &mut self,
        key: &[u8],
        delta: i64,
        time: CacheTime,
    ) -> Result<i64, DurableError> {
        self.healthy()?;
        self.validate_time(time)?;
        self.observe_key(key, time.monotonic_ms)?;
        let ttl = match self.store.ttl(key, time.monotonic_ms) {
            CacheTtl::RemainingMs(ms) => Some(ms),
            _ => None,
        };
        let base = self
            .store
            .get(key, time.monotonic_ms)
            .map(parse_cache_integer)
            .transpose()?
            .unwrap_or(0);
        let next = base
            .checked_add(delta)
            .ok_or(CacheIncrementError::Overflow)?;
        let payload = encode(
            &Mutation::Integer(key, next, absolute(ttl, time)?),
            self.limits.max_record_bytes,
        )?;
        self.commit(&payload, time)?;
        Ok(next)
    }
    pub fn expire(&mut self, key: &[u8], ttl: u64, time: CacheTime) -> Result<bool, DurableError> {
        self.healthy()?;
        self.validate_time(time)?;
        self.observe(time.monotonic_ms)?;
        let Some(value) = self.store.get(key, time.monotonic_ms) else {
            return Ok(false);
        };
        if ttl == 0 {
            self.delete_many(&[key], time)?;
            return Ok(true);
        }
        let deadline = absolute(Some(ttl), time)?;
        let record = match value {
            CacheValueView::Bytes(bytes) => Mutation::Bytes(key, bytes, deadline),
            CacheValueView::Integer(value) => Mutation::Integer(key, value, deadline),
        };
        let payload = encode(&record, self.limits.max_record_bytes)?;
        self.commit(&payload, time)?;
        Ok(true)
    }
    pub fn get(
        &mut self,
        key: &[u8],
        now_ms: u64,
    ) -> Result<Option<CacheValueView<'_>>, DurableError> {
        self.healthy()?;
        self.observe_key(key, now_ms)?;
        Ok(self.store.get(key, now_ms))
    }
    pub fn exists(&mut self, key: &[u8], now_ms: u64) -> Result<bool, DurableError> {
        self.healthy()?;
        self.observe_key(key, now_ms)?;
        Ok(self.store.exists(key, now_ms))
    }

    #[cfg(feature = "cache-server")]
    pub(super) fn store_for_sizing(&self) -> Result<&CacheStore, DurableError> {
        self.healthy()?;
        Ok(&self.store)
    }

    #[cfg(all(test, feature = "cache-server"))]
    pub(super) fn fail_next_append(&mut self) {
        self.wal.fail_next_append = true;
    }

    pub fn ttl(&mut self, key: &[u8], now_ms: u64) -> Result<CacheTtl, DurableError> {
        self.healthy()?;
        self.observe_key(key, now_ms)?;
        Ok(self.store.ttl(key, now_ms))
    }
    pub fn purge_expired(&mut self, now_ms: u64, budget: usize) -> Result<usize, DurableError> {
        self.healthy()?;
        // Validate the monotonic reading before the sweep can remove keys.
        self.anchored_unix(now_ms)?;
        let removed = self.store.purge_expired(now_ms, budget);
        if removed != 0 {
            self.observe(now_ms)?;
        }
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "nulang-durable-{}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&dir).unwrap();
            Self(dir)
        }
        fn path(&self) -> PathBuf {
            self.0.join("cache.wal")
        }
        fn open(&self, clock: CacheTime) -> DurableCache {
            DurableCache::open(
                self.path(),
                CacheLimits::default(),
                WalLimits::default(),
                WalSync::Data,
                clock,
            )
            .unwrap()
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn time(monotonic_ms: u64, unix_ms: u64) -> CacheTime {
        CacheTime {
            monotonic_ms,
            unix_ms,
        }
    }

    #[test]
    fn test_durable_recovery_preserves_absolute_ttl_and_integer_after_image() {
        let tmp = Temp::new();
        let mut cache = tmp.open(time(0, 1_000));
        cache
            .set_bytes(b"n", b"41", Some(500), time(0, 1_000))
            .unwrap();
        assert_eq!(cache.increment(b"n", 1, time(100, 1_100)).unwrap(), 42);
        cache.expire(b"n", 1_000, time(200, 1_200)).unwrap();
        cache
            .set_bytes(b"expired", b"no", Some(10), time(200, 1_200))
            .unwrap();
        cache
            .set_batch(
                &[(b"a", b"\0\xff"), (b"b", b"x"), (b"b", b"last")],
                time(200, 1_200),
            )
            .unwrap();
        assert_eq!(
            cache
                .delete_many(&[b"a", b"a", b"missing"], time(210, 1_210))
                .unwrap(),
            1
        );
        drop(cache);
        let mut recovered = tmp.open(time(10, 1_600));
        assert_eq!(
            recovered.get(b"n", 10).unwrap(),
            Some(CacheValueView::Integer(42))
        );
        assert_eq!(recovered.ttl(b"n", 10).unwrap(), CacheTtl::RemainingMs(600));
        assert_eq!(recovered.get(b"expired", 10).unwrap(), None);
        assert_eq!(recovered.get(b"a", 10).unwrap(), None);
        assert_eq!(
            recovered.get(b"b", 10).unwrap(),
            Some(CacheValueView::Bytes(b"last"))
        );
        assert_eq!(recovered.get(b"n", 610).unwrap(), None);
    }

    #[test]
    fn test_durable_admission_rejects_entire_batch_without_journal_change() {
        let tmp = Temp::new();
        let limits = CacheLimits {
            max_entries: 1,
            max_arena_bytes: 64,
        };
        let mut cache = DurableCache::open(
            tmp.path(),
            limits,
            WalLimits::default(),
            WalSync::Data,
            time(0, 1),
        )
        .unwrap();
        cache
            .set_bytes(b"a", b"original", None, time(0, 1))
            .unwrap();
        let journal = std::fs::read(tmp.path()).unwrap();
        assert!(cache
            .set_batch(&[(b"a", b"changed"), (b"b", b"new")], time(0, 1))
            .is_err());
        assert_eq!(
            cache.get(b"a", 0).unwrap(),
            Some(CacheValueView::Bytes(b"original"))
        );
        assert_eq!(std::fs::read(tmp.path()).unwrap(), journal);
        assert!(!cache.is_poisoned());
    }

    #[test]
    fn test_durable_wal_limit_rejects_before_store_mutation() {
        let tmp = Temp::new();
        let limits = WalLimits {
            max_record_bytes: 128,
            max_file_bytes: 96,
        };
        let mut cache = DurableCache::open(
            tmp.path(),
            CacheLimits::default(),
            limits,
            WalSync::Data,
            time(0, 1),
        )
        .unwrap();
        cache.set_bytes(b"k", b"v", None, time(0, 1)).unwrap();
        let journal = std::fs::read(tmp.path()).unwrap();
        assert!(cache.set_bytes(b"k", b"new", None, time(1, 2)).is_err());
        assert_eq!(
            cache.get(b"k", 1).unwrap(),
            Some(CacheValueView::Bytes(b"v"))
        );
        assert_eq!(std::fs::read(tmp.path()).unwrap(), journal);
        assert!(!cache.is_poisoned());
    }

    #[test]
    fn test_durable_partial_write_poison_blocks_reads_and_writes_until_recovery() {
        use std::io::Write;
        let tmp = Temp::new();
        let mut cache = tmp.open(time(0, 1_000));
        cache.set_bytes(b"k", b"old", None, time(0, 1_000)).unwrap();
        let payload = encode(&Mutation::Bytes(b"k", b"new", None), 128).unwrap();
        let error = cache.commit_with(&payload, time(1, 1_001), |wal, payload, sync| {
            wal.append_with(payload, sync, |file, header, _, _, _| {
                file.write_all(&header[..4])?;
                Err(io::Error::other("injected partial journal write"))
            })
        });
        assert!(error.is_err());
        assert!(cache.is_poisoned());
        assert_eq!(cache.last_sequence(), 1);
        assert!(matches!(cache.get(b"k", 1), Err(DurableError::Poisoned)));
        assert!(matches!(cache.ttl(b"k", 1), Err(DurableError::Poisoned)));
        assert!(matches!(
            cache.increment(b"k", 1, time(1, 1_001)),
            Err(DurableError::Poisoned)
        ));
        assert!(matches!(
            cache.set_bytes(b"k", b"next", None, time(1, 1_001)),
            Err(DurableError::Poisoned)
        ));
        drop(cache);
        let mut recovered = tmp.open(time(0, 1_010));
        assert_eq!(
            recovered.get(b"k", 0).unwrap(),
            Some(CacheValueView::Bytes(b"old"))
        );
        assert_eq!(recovered.last_sequence(), 1);
    }

    #[test]
    fn test_durable_uncertain_complete_record_replays_atomic_batch() {
        use std::io::Write;
        let tmp = Temp::new();
        let mut cache = tmp.open(time(0, 100));
        let payload = encode(&Mutation::Batch(vec![(b"a", b"one"), (b"b", b"two")]), 128).unwrap();
        assert!(cache
            .commit_with(&payload, time(1, 101), |wal, payload, sync| {
                wal.append_with(payload, sync, |file, header, payload, checksum, _| {
                    file.write_all(header)?;
                    file.write_all(payload)?;
                    file.write_all(checksum)?;
                    Err(io::Error::other("injected sync failure"))
                })
            })
            .is_err());
        assert!(cache.is_poisoned());
        assert!(matches!(cache.get(b"a", 1), Err(DurableError::Poisoned)));
        drop(cache);
        let mut recovered = tmp.open(time(0, 200));
        assert_eq!(recovered.last_sequence(), 1);
        assert_eq!(
            recovered.get(b"a", 0).unwrap(),
            Some(CacheValueView::Bytes(b"one"))
        );
        assert_eq!(
            recovered.get(b"b", 0).unwrap(),
            Some(CacheValueView::Bytes(b"two"))
        );
    }

    #[test]
    fn test_durable_decoder_rejects_malformed_payloads_and_trailing_bytes() {
        let good = encode(&Mutation::Batch(vec![(b"k", b"v")]), 128).unwrap();
        for cut in 0..good.len() {
            assert!(decode(&good[..cut]).is_err(), "cut {cut}");
        }
        let mut trailing = good.clone();
        trailing.push(0);
        assert!(decode(&trailing).is_err());
        assert!(decode(&[1, 2, 255, 255, 255, 255]).is_err());
        assert!(decode(&[2, 0]).is_err());
        let tmp = Temp::new();
        let mut wal = CacheWal::open(tmp.path(), WalLimits::default(), |_, _| Ok(())).unwrap();
        wal.append(&good, WalSync::Data).unwrap();
        wal.append(&trailing, WalSync::Data).unwrap();
        drop(wal);
        let original = std::fs::read(tmp.path()).unwrap();
        assert!(DurableCache::open(
            tmp.path(),
            CacheLimits::default(),
            WalLimits::default(),
            WalSync::Data,
            time(0, 100)
        )
        .is_err());
        assert_eq!(std::fs::read(tmp.path()).unwrap(), original);
    }

    #[test]
    fn test_durable_invalid_increment_and_time_overflow_leave_journal_unchanged() {
        let tmp = Temp::new();
        let mut cache = tmp.open(time(0, 100));
        cache.set_bytes(b"k", b"+1", None, time(0, 100)).unwrap();
        let original = std::fs::read(tmp.path()).unwrap();
        assert!(matches!(
            cache.increment(b"k", 1, time(0, 100)),
            Err(DurableError::Increment(CacheIncrementError::NotInteger))
        ));
        assert!(matches!(
            cache.set_bytes(b"k", b"next", Some(1), time(0, u64::MAX)),
            Err(DurableError::InvalidTime)
        ));
        assert_eq!(
            cache.get(b"k", 0).unwrap(),
            Some(CacheValueView::Bytes(b"+1"))
        );
        assert_eq!(std::fs::read(tmp.path()).unwrap(), original);
    }

    #[test]
    fn test_checkpoint_reclaims_history_and_preserves_typed_live_values() {
        let tmp = Temp::new();
        let mut cache = tmp.open(time(0, 1000));
        for n in 0..100 {
            cache
                .set_bytes(b"hot", n.to_string().as_bytes(), None, time(n, 1000 + n))
                .unwrap();
        }
        cache
            .set_bytes(b"n", b"41", Some(1000), time(100, 1100))
            .unwrap();
        cache.increment(b"n", 1, time(100, 1100)).unwrap();
        cache
            .set_bytes(b"expired", b"old", Some(1), time(100, 1100))
            .unwrap();
        cache
            .set_bytes(b"gone", b"old", None, time(100, 1100))
            .unwrap();
        cache.delete_many(&[b"gone"], time(100, 1100)).unwrap();
        let before = std::fs::metadata(tmp.path()).unwrap().len();
        let sequence = cache.last_sequence();
        let estimated = cache.estimated_checkpoint_bytes(200);
        let report = cache.checkpoint(time(200, 1200)).unwrap();
        assert_eq!(estimated, report.after_bytes);
        assert_eq!(report.entries, 2);
        assert!(report.after_bytes < before / 10);
        assert!(cache.last_sequence() >= sequence);
        cache
            .set_bytes(b"later", b"yes", None, time(201, 1201))
            .unwrap();
        drop(cache);
        let mut recovered = tmp.open(time(0, 1300));
        assert_eq!(
            recovered.get(b"hot", 0).unwrap(),
            Some(CacheValueView::Bytes(b"99"))
        );
        assert_eq!(
            recovered.get(b"n", 0).unwrap(),
            Some(CacheValueView::Integer(42))
        );
        assert_eq!(recovered.ttl(b"n", 0).unwrap(), CacheTtl::RemainingMs(800));
        assert_eq!(
            recovered.get(b"later", 0).unwrap(),
            Some(CacheValueView::Bytes(b"yes"))
        );
        assert_eq!(recovered.get(b"expired", 0).unwrap(), None);
        assert_eq!(recovered.get(b"gone", 0).unwrap(), None);
    }

    #[test]
    fn test_checkpoint_recovers_capacity_after_journal_fills() {
        let tmp = Temp::new();
        let limits = WalLimits {
            max_record_bytes: 128,
            max_file_bytes: 240,
        };
        let mut cache = DurableCache::open(
            tmp.path(),
            CacheLimits::default(),
            limits,
            WalSync::Data,
            time(0, 100),
        )
        .unwrap();
        for _ in 0..3 {
            cache.set_bytes(b"k", b"v", None, time(0, 100)).unwrap();
        }
        assert!(cache.set_bytes(b"k", b"more", None, time(0, 100)).is_err());
        cache.checkpoint(time(0, 100)).unwrap();
        cache.set_bytes(b"k", b"more", None, time(0, 100)).unwrap();
        drop(cache);
        let mut recovered = DurableCache::open(
            tmp.path(),
            CacheLimits::default(),
            limits,
            WalSync::Data,
            time(0, 100),
        )
        .unwrap();
        assert_eq!(
            recovered.get(b"k", 0).unwrap(),
            Some(CacheValueView::Bytes(b"more"))
        );
    }

    #[test]
    fn clock_rollback_after_observed_expiry_cannot_resurrect_key() {
        let tmp = Temp::new();
        let mut cache = tmp.open(time(0, 1000));
        cache
            .set_bytes(b"ttl", b"value", Some(100), time(0, 1000))
            .unwrap();
        assert_eq!(cache.get(b"ttl", 101).unwrap(), None);
        drop(cache);
        assert!(DurableCache::open(
            tmp.path(),
            CacheLimits::default(),
            WalLimits::default(),
            WalSync::Data,
            time(0, 1050)
        )
        .is_err());
        let mut recovered = tmp.open(time(0, 1101));
        assert_eq!(recovered.get(b"ttl", 0).unwrap(), None);
    }

    #[test]
    fn clock_rollback_after_ttl_observation_cannot_extend_remaining_ttl() {
        let tmp = Temp::new();
        let mut cache = tmp.open(time(0, 1000));
        cache
            .set_bytes(b"ttl", b"value", Some(1000), time(0, 1000))
            .unwrap();
        assert_eq!(cache.ttl(b"ttl", 400).unwrap(), CacheTtl::RemainingMs(600));
        drop(cache);
        assert!(DurableCache::open(
            tmp.path(),
            CacheLimits::default(),
            WalLimits::default(),
            WalSync::Data,
            time(0, 1300)
        )
        .is_err());
        let mut recovered = tmp.open(time(0, 1400));
        assert_eq!(
            recovered.ttl(b"ttl", 0).unwrap(),
            CacheTtl::RemainingMs(600)
        );
    }

    fn try_open(tmp: &Temp, now: u64) -> Result<DurableCache, DurableError> {
        DurableCache::open(
            tmp.path(),
            CacheLimits::default(),
            WalLimits::default(),
            WalSync::Data,
            time(0, now),
        )
    }

    #[test]
    fn clock_floor_survives_sweep_checkpoint_and_reopen() {
        let tmp = Temp::new();
        let mut cache = tmp.open(time(0, 1000));
        cache
            .set_bytes(b"expired", b"gone", Some(10), time(0, 1000))
            .unwrap();
        cache
            .set_bytes(b"live", b"value", Some(1000), time(0, 1000))
            .unwrap();
        assert_eq!(cache.purge_expired(200, 100).unwrap(), 1);
        cache.checkpoint(time(400, 1400)).unwrap();
        drop(cache);
        assert!(try_open(&tmp, 1399).is_err());
        let mut recovered = try_open(&tmp, 1400).unwrap();
        assert_eq!(recovered.get(b"expired", 0).unwrap(), None);
        assert_eq!(
            recovered.ttl(b"live", 0).unwrap(),
            CacheTtl::RemainingMs(600)
        );
    }

    #[test]
    fn corrupt_or_missing_clock_floor_refuses_recovery() {
        let tmp = Temp::new();
        let mut cache = tmp.open(time(0, 1000));
        cache
            .set_bytes(b"ttl", b"value", Some(100), time(0, 1000))
            .unwrap();
        drop(cache);
        let clock = tmp.0.join("cache.wal.clock");
        let original = std::fs::read(&clock).unwrap();
        let mut corrupt = original.clone();
        corrupt[9] ^= 1;
        std::fs::write(&clock, corrupt).unwrap();
        assert!(try_open(&tmp, 1200).is_err());
        std::fs::remove_file(&clock).unwrap();
        assert!(try_open(&tmp, 1200).is_err());
        std::fs::write(&clock, original).unwrap();
        assert!(try_open(&tmp, 1200).is_ok());
    }

    #[test]
    fn full_journal_still_records_clock_floor_for_expiration() {
        let tmp = Temp::new();
        let limits = WalLimits {
            max_record_bytes: 128,
            max_file_bytes: 96,
        };
        let open = |now| {
            DurableCache::open(
                tmp.path(),
                CacheLimits::default(),
                limits,
                WalSync::Data,
                time(0, now),
            )
        };
        let mut cache = open(1000).unwrap();
        cache
            .set_bytes(b"k", b"v", Some(10), time(0, 1000))
            .unwrap();
        assert!(cache
            .set_bytes(b"k", b"v", Some(10), time(0, 1000))
            .is_err());
        let journal = std::fs::read(tmp.path()).unwrap();
        assert_eq!(cache.get(b"k", 11).unwrap(), None);
        assert_eq!(std::fs::read(tmp.path()).unwrap(), journal);
        drop(cache);
        assert!(open(1010).is_err());
        assert_eq!(open(1011).unwrap().get(b"k", 0).unwrap(), None);
    }

    #[test]
    fn clock_publication_failure_blocks_reads_and_writes() {
        let tmp = Temp::new();
        let mut cache = tmp.open(time(0, 1000));
        cache
            .set_bytes(b"k", b"v", Some(10), time(0, 1000))
            .unwrap();
        let clock = tmp.0.join("cache.wal.clock");
        std::fs::remove_file(&clock).unwrap();
        std::fs::create_dir(&clock).unwrap();
        assert!(cache.get(b"k", 11).is_err());
        assert!(cache.is_poisoned());
        assert!(cache.set_bytes(b"x", b"y", None, time(12, 1012)).is_err());
        assert!(cache.get(b"k", 12).is_err());
    }

    #[test]
    fn inconsistent_native_time_pair_cannot_change_ttl() {
        let tmp = Temp::new();
        let mut cache = tmp.open(time(50, 1000));
        cache
            .set_bytes(b"k", b"v", Some(100), time(50, 1000))
            .unwrap();
        let old = std::fs::read(tmp.path()).unwrap();
        assert!(matches!(
            cache.expire(b"k", 200, time(60, 999)),
            Err(DurableError::InvalidTime)
        ));
        assert_eq!(std::fs::read(tmp.path()).unwrap(), old);
        assert_eq!(cache.ttl(b"k", 60).unwrap(), CacheTtl::RemainingMs(90));
    }

    #[test]
    fn startup_expiration_advances_floor_before_replay_is_published() {
        let tmp = Temp::new();
        let mut cache = tmp.open(time(0, 1000));
        cache
            .set_bytes(b"ttl", b"value", Some(100), time(0, 1000))
            .unwrap();
        drop(cache);
        let mut recovered = try_open(&tmp, 1200).unwrap();
        assert_eq!(recovered.get(b"ttl", 0).unwrap(), None);
        drop(recovered);
        assert!(try_open(&tmp, 1050).is_err());
    }

    #[test]
    fn clock_reservation_covers_observations_without_rewriting_floor() {
        let tmp = Temp::new();
        let mut cache = tmp.open(time(0, 1000));
        cache.configure_clock_reservation(250).unwrap();
        cache
            .set_bytes(b"k", b"v", Some(500), time(10, 1010))
            .unwrap();
        let path = tmp.0.join("cache.wal.clock");
        let original = std::fs::read(&path).unwrap();
        assert_eq!(
            u64::from_le_bytes(original[8..16].try_into().unwrap()),
            1260
        );
        assert_eq!(cache.ttl(b"k", 100).unwrap(), CacheTtl::RemainingMs(410));
        assert_eq!(cache.ttl(b"k", 260).unwrap(), CacheTtl::RemainingMs(250));
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert_eq!(
            cache.get(b"k", 261).unwrap(),
            Some(CacheValueView::Bytes(b"v"))
        );
        let renewed = std::fs::read(&path).unwrap();
        assert_eq!(u64::from_le_bytes(renewed[8..16].try_into().unwrap()), 1511);
        drop(cache);
        assert!(try_open(&tmp, 1510).is_err());
        assert_eq!(try_open(&tmp, 1511).unwrap().get(b"k", 0).unwrap(), None);
    }

    #[test]
    fn reserved_floor_does_not_permit_in_process_regression() {
        let tmp = Temp::new();
        let mut cache = tmp.open(time(0, 1000));
        cache.configure_clock_reservation(250).unwrap();
        cache
            .set_bytes(b"k", b"v", Some(1000), time(10, 1010))
            .unwrap();
        assert_eq!(cache.ttl(b"k", 100).unwrap(), CacheTtl::RemainingMs(910));
        assert!(cache.ttl(b"k", 99).is_err());
        assert!(cache.is_poisoned());
    }

    #[test]
    fn reserved_floor_survives_expiry_and_fails_closed_at_renewal() {
        let tmp = Temp::new();
        let mut cache = tmp.open(time(0, 1000));
        cache.configure_clock_reservation(250).unwrap();
        cache
            .set_bytes(b"k", b"v", Some(10), time(10, 1010))
            .unwrap();
        assert_eq!(cache.get(b"k", 21).unwrap(), None);
        drop(cache);
        assert!(try_open(&tmp, 1021).is_err());
        let mut cache = try_open(&tmp, 1260).unwrap();
        assert_eq!(cache.get(b"k", 0).unwrap(), None);
        cache.configure_clock_reservation(250).unwrap();
        cache
            .set_bytes(b"live", b"v", Some(1000), time(1, 1261))
            .unwrap();
        let path = tmp.0.join("cache.wal.clock");
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(cache.get(b"live", 252).is_err());
        assert!(cache.is_poisoned());
    }
}
