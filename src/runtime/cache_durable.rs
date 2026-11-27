//! Single-owner native durable cache. Mutation records contain after-images
//! with absolute Unix deadlines, never process-relative timestamps.
//! Optional durable reactors use this owner; memory mode bypasses it.

use super::cache::{
    parse_cache_integer, CacheAdmissionError, CacheIncrementError, CacheLimits, CacheStore,
    CacheTtl, CacheValueView,
};
use super::cache_wal::{CacheWal, CheckpointReport, WalLimits, WalSync};
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
    limits: WalLimits,
    sync: WalSync,
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
        let wal = CacheWal::open(path, limits, |_, bytes| {
            let record = decode(bytes)?;
            record.apply(&mut store, time).map_err(io::Error::other)?;
            Ok(())
        })?;
        Ok(Self {
            store,
            wal,
            limits,
            sync,
            poisoned: false,
        })
    }
    /// Publish a complete snapshot as the new journal prefix, then continue
    /// appending to it. One entry is encoded at a time; no whole-cache clone.
    pub fn checkpoint(&mut self, time: CacheTime) -> Result<CheckpointReport, DurableError> {
        self.healthy()?;
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
        let record = decode(payload)?;
        record.preflight(&self.store, time)?;
        // Poison before the append so unwinding cannot expose uncertain state.
        self.poisoned = true;
        if let Err(error) = append(&mut self.wal, payload, self.sync) {
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
        Ok(self.store.get(key, now_ms))
    }
    pub fn exists(&mut self, key: &[u8], now_ms: u64) -> Result<bool, DurableError> {
        self.healthy()?;
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
        Ok(self.store.ttl(key, now_ms))
    }
    pub fn purge_expired(&mut self, now_ms: u64, budget: usize) -> Result<usize, DurableError> {
        self.healthy()?;
        Ok(self.store.purge_expired(now_ms, budget))
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
        let report = cache.checkpoint(time(200, 1200)).unwrap();
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
}
