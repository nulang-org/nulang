//! Single-writer journal framing for the cache persistence layer.
//!
//! Each opaque payload is one atomic logical record. The caller must validate
//! and encode mutations before appending, and must not publish recovered state
//! until open succeeds. Compaction publishes a complete checkpoint journal atomically.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const MAGIC: &[u8; 8] = b"NLCWAL02";
const HEADER_BYTES: usize = MAGIC.len();
const CHECKPOINT_MAGIC: &[u8; 8] = b"NLCWAL03";
const CHECKPOINT_HEADER_BYTES: usize = 8 + 8 + 8 + 32;

const RECORD_HEADER_BYTES: usize = 16;
const CHECKSUM_BYTES: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WalLimits {
    pub max_record_bytes: usize,
    pub max_file_bytes: u64,
}

impl Default for WalLimits {
    fn default() -> Self {
        Self {
            max_record_bytes: 8 * 1024 * 1024,
            max_file_bytes: 1024 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WalSync {
    /// Acknowledged by the OS, not necessarily persisted to disk.
    Buffered,
    /// Sync file data before acknowledging the record.
    Data,
}

#[derive(Debug)]
pub struct CacheWal {
    file: File,
    path: PathBuf,
    lock: File,
    limits: WalLimits,
    last_sequence: u64,
    valid_len: u64,
    poisoned: bool,
    #[cfg(all(test, feature = "cache-server"))]
    pub(super) fail_next_append: bool,
    #[cfg(all(test, feature = "cache-server"))]
    pub(super) fail_next_sync: bool,
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn checksum(header: &[u8; RECORD_HEADER_BYTES], payload: &[u8]) -> [u8; CHECKSUM_BYTES] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(header);
    hasher.update(payload);
    *hasher.finalize().as_bytes()
}

fn parent_dir(path: &Path) -> &Path {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}

fn checkpoint_header(base: u64, count: u64) -> [u8; CHECKPOINT_HEADER_BYTES] {
    let mut header = [0; CHECKPOINT_HEADER_BYTES];
    header[..8].copy_from_slice(CHECKPOINT_MAGIC);
    header[8..16].copy_from_slice(&base.to_le_bytes());
    header[16..24].copy_from_slice(&count.to_le_bytes());
    let digest = blake3::hash(&header[..24]);
    header[24..].copy_from_slice(digest.as_bytes());
    header
}

struct TemporaryJournal(PathBuf);
impl Drop for TemporaryJournal {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckpointReport {
    pub entries: u64,
    pub before_bytes: u64,
    pub after_bytes: u64,
}

/// A tiny independently synced clock floor, coordinated by the WAL's stable
/// writer lock. Keeping this out of the bounded journal permits safe TTL reads
/// even when the mutation journal is full. Never restore the WAL alone.
#[derive(Debug)]
pub(super) struct ClockFloor {
    path: PathBuf,
    unix_ms: u64,
    observed_ms: u64,
    reservation_ms: u64,
}

/// Maximum opt-in advance reservation; bounds additional restart delay.
pub const MAX_CLOCK_RESERVATION_MS: u64 = 1000;

const CLOCK_MAGIC: &[u8; 8] = b"NLCLK001";

impl ClockFloor {
    fn path_for(journal: &Path) -> PathBuf {
        let mut path = journal.as_os_str().to_os_string();
        path.push(".clock");
        PathBuf::from(path)
    }

    fn encode(unix_ms: u64) -> [u8; 48] {
        let mut record = [0; 48];
        record[..8].copy_from_slice(CLOCK_MAGIC);
        record[8..16].copy_from_slice(&unix_ms.to_le_bytes());
        let digest = blake3::hash(&record[..16]);
        record[16..].copy_from_slice(digest.as_bytes());
        record
    }

    /// Called exclusively while holding the stable lock, before replay.
    pub(super) fn open(journal: &Path, lock: &mut File, now: u64) -> io::Result<Self> {
        let path = Self::path_for(journal);
        let marker_len = lock.metadata()?.len();
        if marker_len != 0 {
            let mut marker = [0; 8];
            if marker_len != 8 {
                return Err(invalid("invalid cache clock lock marker"));
            }
            lock.seek(SeekFrom::Start(0))?;
            lock.read_exact(&mut marker)?;
            if &marker != CLOCK_MAGIC {
                return Err(invalid("invalid cache clock lock marker"));
            }
        }
        let previous = match File::open(&path) {
            Ok(mut file) => {
                let mut record = [0; 48];
                if file.metadata()?.len() != record.len() as u64 {
                    return Err(invalid("invalid cache clock floor length"));
                }
                file.read_exact(&mut record)?;
                let floor = u64::from_le_bytes(record[8..16].try_into().unwrap());
                if record != Self::encode(floor) {
                    return Err(invalid("invalid cache clock floor checksum"));
                }
                Some(floor)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound && marker_len == 0 => None,
            Err(error) => return Err(error),
        };
        if previous.is_some_and(|floor| now < floor) {
            return Err(invalid(
                "cache wall clock regressed below durable floor; correct clock before recovery",
            ));
        }
        let mut clock = Self {
            path,
            unix_ms: previous.unwrap_or(0),
            observed_ms: now,
            reservation_ms: 0,
        };
        if previous.is_none() || previous != Some(now) {
            clock.publish(now)?;
        }
        // Marker is durable only after the initial floor is durable. If the
        // floor disappears later, refuse recovery rather than reset it.
        if marker_len == 0 {
            lock.seek(SeekFrom::Start(0))?;
            lock.write_all(CLOCK_MAGIC)?;
            lock.sync_all()?;
        }
        Ok(clock)
    }

    pub(super) fn configure_reservation(&mut self, millis: u64) -> io::Result<()> {
        if millis > MAX_CLOCK_RESERVATION_MS {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "clock reservation exceeds maximum",
            ));
        }
        self.reservation_ms = millis;
        Ok(())
    }

    pub(super) fn advance(&mut self, now: u64) -> io::Result<()> {
        if now < self.observed_ms {
            return Err(invalid("cache clock moved backwards"));
        }
        if now > self.unix_ms {
            // A persisted upper watermark covers every acknowledged
            // observation. Recovery still refuses any wall time below it.
            self.publish(now.saturating_add(self.reservation_ms))?;
        }
        self.observed_ms = now;
        Ok(())
    }

    fn publish(&mut self, now: u64) -> io::Result<()> {
        use std::os::unix::fs::OpenOptionsExt;
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let parent = parent_dir(&self.path);
        let directory = File::open(parent)?;
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(io::Error::other)?
            .as_nanos();
        let path = parent.join(format!(
            ".nulang-clock-{}-{nonce}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        let temp = TemporaryJournal(path);
        file.write_all(&Self::encode(now))?;
        file.sync_all()?;
        std::fs::rename(&temp.0, &self.path)?;
        directory.sync_all()?;
        self.unix_ms = now;
        Ok(())
    }
}

impl CacheWal {
    /// Open or exclusively create a journal, stream validated records to a
    /// staging callback, and repair an incomplete final record only. The
    /// caller must discard staging state if any callback or validation fails.
    /// Exclusive advisory locking is held for the entire handle lifetime.
    pub fn open(
        path: impl AsRef<Path>,
        limits: WalLimits,
        on_record: impl FnMut(u64, &[u8]) -> io::Result<()>,
    ) -> io::Result<Self> {
        Self::open_prepared(path, limits, |_, _| Ok(()), on_record)
    }

    pub(super) fn open_prepared(
        path: impl AsRef<Path>,
        limits: WalLimits,
        prepare: impl FnOnce(&Path, &mut File) -> io::Result<()>,
        mut on_record: impl FnMut(u64, &[u8]) -> io::Result<()>,
    ) -> io::Result<Self> {
        if limits.max_record_bytes == 0
            || limits.max_record_bytes > u32::MAX as usize
            || limits.max_file_bytes < HEADER_BYTES as u64
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid cache WAL limits",
            ));
        }
        let path = path.as_ref();
        // Stable advisory lock survives journal inode replacement. Never
        // unlink this sidecar while any cooperating writer may be active.
        let mut lock_path = path.as_os_str().to_os_string();
        lock_path.push(".lock");
        let mut lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path)?;
        lock.try_lock().map_err(io::Error::other)?;
        let (mut file, created) = match OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)
        {
            Ok(file) => (file, true),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                (OpenOptions::new().read(true).write(true).open(path)?, false)
            }
            Err(error) => return Err(error),
        };
        file.try_lock().map_err(io::Error::other)?;
        if created {
            file.write_all(MAGIC)?;
            file.sync_all()?;
        }
        // Sync the directory entry as well as the journal contents, including
        // a retry after an earlier failed creation sync.
        File::open(parent_dir(path))?.sync_all()?;
        let length = file.metadata()?.len();
        if length < HEADER_BYTES as u64 || length > limits.max_file_bytes {
            return Err(invalid("cache WAL file length outside limits"));
        }
        file.seek(SeekFrom::Start(0))?;
        let mut magic = [0; HEADER_BYTES];
        file.read_exact(&mut magic)?;
        let (mut position, mut last_sequence, mut required_records) = if &magic == MAGIC {
            (HEADER_BYTES as u64, 0u64, 0u64)
        } else if &magic == CHECKPOINT_MAGIC {
            let mut rest = [0; CHECKPOINT_HEADER_BYTES - HEADER_BYTES];
            file.read_exact(&mut rest)?;
            let base = u64::from_le_bytes(rest[..8].try_into().unwrap());
            let count = u64::from_le_bytes(rest[8..16].try_into().unwrap());
            if rest[16..] != checkpoint_header(base, count)[24..]
                || base.checked_add(count).is_none()
            {
                return Err(invalid("invalid cache checkpoint header"));
            }
            (CHECKPOINT_HEADER_BYTES as u64, base, count)
        } else {
            return Err(invalid("unsupported cache WAL magic/version"));
        };
        prepare(path, &mut lock)?;
        let mut payload = Vec::new();
        while length - position >= RECORD_HEADER_BYTES as u64 {
            let mut header = [0; RECORD_HEADER_BYTES];
            file.read_exact(&mut header)?;
            let size = u32::from_le_bytes(header[..4].try_into().unwrap());
            let complement = u32::from_le_bytes(header[4..8].try_into().unwrap());
            let seq = u64::from_le_bytes(header[8..].try_into().unwrap());
            if size ^ u32::MAX != complement || size as usize > limits.max_record_bytes {
                return Err(invalid("invalid cache WAL record length"));
            }
            if last_sequence.checked_add(1) != Some(seq) {
                return Err(invalid("noncontiguous cache WAL sequence"));
            }
            let total = RECORD_HEADER_BYTES as u64 + u64::from(size) + CHECKSUM_BYTES as u64;
            if length - position < total {
                break;
            }
            payload.clear();
            payload
                .try_reserve_exact(size as usize)
                .map_err(io::Error::other)?;
            payload.resize(size as usize, 0);
            file.read_exact(&mut payload)?;
            let mut digest = [0; CHECKSUM_BYTES];
            file.read_exact(&mut digest)?;
            if digest != checksum(&header, &payload) {
                return Err(invalid("cache WAL checksum mismatch"));
            }
            on_record(seq, &payload)?;
            position += total;
            last_sequence = seq;
            required_records = required_records.saturating_sub(1);
        }

        // A checkpoint is a complete required prefix. Never treat missing
        // snapshot entries as a repairable torn append.
        if required_records != 0 {
            return Err(invalid("incomplete cache checkpoint"));
        }
        if position != length {
            file.set_len(position)?;
            file.sync_all()?;
        }
        file.seek(SeekFrom::Start(position))?;
        Ok(Self {
            file,
            path: path.to_path_buf(),
            lock,
            limits,
            last_sequence,
            valid_len: position,
            poisoned: false,
            #[cfg(all(test, feature = "cache-server"))]
            fail_next_append: false,
            #[cfg(all(test, feature = "cache-server"))]
            fail_next_sync: false,
        })
    }

    /// Write a complete checkpoint to a private sibling file, sync it, then
    /// atomically replace the old journal. The stable sidecar lock remains
    /// held across replacement. The callback may only append snapshot records.
    pub(super) fn rotate(
        &mut self,
        entries: u64,
        populate: impl FnOnce(&mut CacheWal) -> io::Result<()>,
    ) -> io::Result<CheckpointReport> {
        self.rotate_with(entries, populate, |_| Ok(()))
    }

    fn rotate_with(
        &mut self,
        entries: u64,
        populate: impl FnOnce(&mut CacheWal) -> io::Result<()>,
        boundary: impl Fn(bool) -> io::Result<()>,
    ) -> io::Result<CheckpointReport> {
        if self.poisoned {
            return Err(io::Error::other("cache WAL poisoned"));
        }
        if self.limits.max_file_bytes < CHECKPOINT_HEADER_BYTES as u64 {
            return Err(io::Error::new(
                io::ErrorKind::StorageFull,
                "checkpoint header exceeds WAL limit",
            ));
        }
        let expected = self
            .last_sequence
            .checked_add(entries)
            .ok_or_else(|| invalid("checkpoint sequence exhausted"))?;
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let parent = parent_dir(&self.path);
        let directory = File::open(parent)?;
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(io::Error::other)?
            .as_nanos();
        let temp_path = parent.join(format!(
            ".nulang-checkpoint-{}-{nonce}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&temp_path)?;
        let temp = TemporaryJournal(temp_path);
        file.try_lock().map_err(io::Error::other)?;
        file.write_all(&checkpoint_header(self.last_sequence, entries))?;
        let mut replacement = CacheWal {
            file,
            path: self.path.clone(),
            lock: self.lock.try_clone()?,
            limits: self.limits,
            last_sequence: self.last_sequence,
            valid_len: CHECKPOINT_HEADER_BYTES as u64,
            poisoned: false,
            #[cfg(all(test, feature = "cache-server"))]
            fail_next_append: false,
            #[cfg(all(test, feature = "cache-server"))]
            fail_next_sync: false,
        };
        populate(&mut replacement)?;
        if replacement.poisoned || replacement.last_sequence != expected {
            return Err(invalid("checkpoint entry count mismatch"));
        }
        replacement.file.sync_all()?;
        boundary(false)?;
        let report = CheckpointReport {
            entries,
            before_bytes: self.valid_len,
            after_bytes: replacement.valid_len,
        };
        // From here on failure is ambiguous, so the original handle must
        // never resume appending to a potentially unlinked inode.
        self.poisoned = true;
        std::fs::rename(&temp.0, &self.path)?;
        boundary(true)?;
        directory.sync_all()?;
        *self = replacement;
        Ok(report)
    }

    pub(super) fn journal_bytes(&self) -> u64 {
        self.valid_len
    }

    pub(super) fn checkpoint_header_bytes() -> u64 {
        CHECKPOINT_HEADER_BYTES as u64
    }

    pub(super) fn framed_record_bytes(payload_bytes: u64) -> u64 {
        payload_bytes.saturating_add((RECORD_HEADER_BYTES + CHECKSUM_BYTES) as u64)
    }

    pub fn last_sequence(&self) -> u64 {
        self.last_sequence
    }
    pub fn is_poisoned(&self) -> bool {
        self.poisoned
    }

    /// Synchronize all acknowledged buffered appends. An uncertain sync
    /// poisons the journal just like a failed synchronized append.
    pub fn synchronize(&mut self) -> io::Result<()> {
        #[cfg(all(test, feature = "cache-server"))]
        if std::mem::take(&mut self.fail_next_sync) {
            return self.synchronize_with(|_| Err(io::Error::other("injected group sync failure")));
        }
        self.synchronize_with(File::sync_data)
    }

    fn synchronize_with(&mut self, sync: impl FnOnce(&File) -> io::Result<()>) -> io::Result<()> {
        if self.poisoned {
            return Err(io::Error::other("cache WAL poisoned"));
        }
        self.poisoned = true;
        sync(&self.file)?;
        self.poisoned = false;
        Ok(())
    }

    pub fn append(&mut self, payload: &[u8], sync: WalSync) -> io::Result<u64> {
        #[cfg(all(test, feature = "cache-server"))]
        if std::mem::take(&mut self.fail_next_append) {
            return self.append_with(payload, sync, |file, header, _, _, _| {
                file.write_all(&header[..4])?;
                Err(io::Error::other("injected partial journal write"))
            });
        }
        self.append_with(payload, sync, |file, header, payload, digest, sync| {
            file.write_all(header)?;
            file.write_all(payload)?;
            file.write_all(digest)?;
            if sync == WalSync::Data {
                file.sync_data()?;
            }
            Ok(())
        })
    }

    pub(super) fn append_with(
        &mut self,
        payload: &[u8],
        sync: WalSync,
        write: impl FnOnce(
            &mut File,
            &[u8; RECORD_HEADER_BYTES],
            &[u8],
            &[u8; CHECKSUM_BYTES],
            WalSync,
        ) -> io::Result<()>,
    ) -> io::Result<u64> {
        if self.poisoned {
            return Err(io::Error::other(
                "cache WAL poisoned; reopen and recover before writes",
            ));
        }
        let size = u32::try_from(payload.len()).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "cache WAL record exceeds u32")
        })?;
        if payload.len() > self.limits.max_record_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "cache WAL record exceeds limit",
            ));
        }
        let seq = self
            .last_sequence
            .checked_add(1)
            .ok_or_else(|| invalid("cache WAL sequence exhausted"))?;
        let next_len = self
            .valid_len
            .checked_add(RECORD_HEADER_BYTES as u64 + u64::from(size) + CHECKSUM_BYTES as u64)
            .filter(|len| *len <= self.limits.max_file_bytes)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::StorageFull, "cache WAL file limit reached")
            })?;
        let mut header = [0; RECORD_HEADER_BYTES];
        header[..4].copy_from_slice(&size.to_le_bytes());
        header[4..8].copy_from_slice(&(size ^ u32::MAX).to_le_bytes());
        header[8..].copy_from_slice(&seq.to_le_bytes());
        let digest = checksum(&header, payload);
        // Poison before any I/O; an error or unwind must not permit appends
        // after a potentially partial record or ambiguous sync.
        self.poisoned = true;
        write(&mut self.file, &header, payload, &digest, sync)?;
        self.valid_len = next_len;
        self.last_sequence = seq;
        self.poisoned = false;
        Ok(seq)
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
            let path = std::env::temp_dir().join(format!(
                "nulang-cache-wal-{}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn file(&self) -> PathBuf {
            self.0.join("journal")
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn test_wal_streaming_roundtrip_and_exclusive_writer() {
        let temp = Temp::new();
        let path = temp.file();
        let mut wal = CacheWal::open(&path, WalLimits::default(), |_, _| Ok(())).unwrap();
        assert_eq!(wal.append(b"first", WalSync::Data).unwrap(), 1);
        assert_eq!(wal.append(b"batch\0\xff", WalSync::Buffered).unwrap(), 2);
        assert!(CacheWal::open(&path, WalLimits::default(), |_, _| Ok(())).is_err());
        drop(wal);
        let mut recovered = Vec::new();
        let wal = CacheWal::open(&path, WalLimits::default(), |seq, payload| {
            recovered.push((seq, payload.to_vec()));
            Ok(())
        })
        .unwrap();
        assert_eq!(
            recovered,
            vec![(1, b"first".to_vec()), (2, b"batch\0\xff".to_vec())]
        );
        assert_eq!(wal.last_sequence(), 2);
    }

    #[test]
    fn test_wal_recovers_every_torn_record_boundary() {
        let temp = Temp::new();
        let path = temp.file();
        let mut wal = CacheWal::open(&path, WalLimits::default(), |_, _| Ok(())).unwrap();
        wal.append(b"stable", WalSync::Data).unwrap();
        let prefix = std::fs::read(&path).unwrap();
        wal.append(b"atomic batch", WalSync::Data).unwrap();
        drop(wal);
        let complete = std::fs::read(&path).unwrap();
        for cut in prefix.len()..complete.len() {
            std::fs::write(&path, &complete[..cut]).unwrap();
            let mut seen = Vec::new();
            let mut wal = CacheWal::open(&path, WalLimits::default(), |seq, _| {
                seen.push(seq);
                Ok(())
            })
            .unwrap();
            assert_eq!(seen, [1], "cut {cut}");
            assert_eq!(std::fs::metadata(&path).unwrap().len(), prefix.len() as u64);
            assert_eq!(wal.append(b"replacement", WalSync::Data).unwrap(), 2);
        }
    }

    #[test]
    fn test_wal_corruption_fails_closed_without_truncation() {
        let temp = Temp::new();
        let path = temp.file();
        let mut wal = CacheWal::open(&path, WalLimits::default(), |_, _| Ok(())).unwrap();
        wal.append(b"record", WalSync::Data).unwrap();
        drop(wal);
        let original = std::fs::read(&path).unwrap();
        for offset in [
            0,
            HEADER_BYTES,
            HEADER_BYTES + 4,
            HEADER_BYTES + 8,
            HEADER_BYTES + RECORD_HEADER_BYTES,
            original.len() - 1,
        ] {
            let mut corrupt = original.clone();
            corrupt[offset] ^= 1;
            std::fs::write(&path, &corrupt).unwrap();
            assert!(
                CacheWal::open(&path, WalLimits::default(), |_, _| Ok(())).is_err(),
                "offset {offset}"
            );
            assert_eq!(std::fs::read(&path).unwrap(), corrupt);
        }
    }

    #[test]
    fn test_wal_limits_and_poison_after_partial_write() {
        let temp = Temp::new();
        let path = temp.file();
        let limits = WalLimits {
            max_record_bytes: 8,
            max_file_bytes: 256,
        };
        let mut wal = CacheWal::open(&path, limits, |_, _| Ok(())).unwrap();
        assert!(wal.append(b"too large!", WalSync::Data).is_err());
        assert!(!wal.is_poisoned());
        wal.append(b"ok", WalSync::Data).unwrap();
        let result = wal.append_with(b"bad", WalSync::Data, |file, header, _, _, _| {
            file.write_all(&header[..4])?;
            Err(std::io::Error::other("injected partial write"))
        });
        assert!(result.is_err());
        assert!(wal.is_poisoned());
        assert_eq!(wal.last_sequence(), 1);
        assert!(wal.append(b"next", WalSync::Data).is_err());
        drop(wal);
        let mut seen = Vec::new();
        let wal = CacheWal::open(&path, limits, |seq, _| {
            seen.push(seq);
            Ok(())
        })
        .unwrap();
        assert_eq!(seen, [1]);
        assert_eq!(wal.last_sequence(), 1);
    }

    #[test]
    fn test_wal_sync_failure_is_ambiguous_and_poisoned() {
        let temp = Temp::new();
        let path = temp.file();
        let mut wal = CacheWal::open(&path, WalLimits::default(), |_, _| Ok(())).unwrap();
        wal.append(b"known", WalSync::Data).unwrap();
        let error = wal.append_with(
            b"uncertain",
            WalSync::Data,
            |file, header, payload, digest, _| {
                file.write_all(header)?;
                file.write_all(payload)?;
                file.write_all(digest)?;
                Err(io::Error::other("injected sync failure"))
            },
        );
        assert!(error.is_err());
        assert!(wal.is_poisoned());
        assert_eq!(wal.last_sequence(), 1);
        assert!(wal.append(b"not allowed", WalSync::Buffered).is_err());
        drop(wal);
        let mut seen = Vec::new();
        let mut wal = CacheWal::open(&path, WalLimits::default(), |seq, payload| {
            seen.push((seq, payload.to_vec()));
            Ok(())
        })
        .unwrap();
        assert_eq!(seen, [(1, b"known".to_vec()), (2, b"uncertain".to_vec())]);
        assert_eq!(wal.append(b"next", WalSync::Data).unwrap(), 3);
    }

    #[test]
    fn test_wal_file_limit_rejects_before_writing() {
        let temp = Temp::new();
        let path = temp.file();
        let limits = WalLimits {
            max_record_bytes: 8,
            max_file_bytes: 60,
        };
        let mut wal = CacheWal::open(&path, limits, |_, _| Ok(())).unwrap();
        wal.append(b"four", WalSync::Data).unwrap();
        let original = std::fs::read(&path).unwrap();
        assert_eq!(original.len(), 60);
        assert_eq!(
            wal.append(b"", WalSync::Buffered).unwrap_err().kind(),
            io::ErrorKind::StorageFull
        );
        assert!(!wal.is_poisoned());
        assert_eq!(wal.last_sequence(), 1);
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }

    #[test]
    fn test_wal_replay_callback_error_does_not_modify_journal() {
        let temp = Temp::new();
        let path = temp.file();
        let mut wal = CacheWal::open(&path, WalLimits::default(), |_, _| Ok(())).unwrap();
        wal.append(b"one", WalSync::Data).unwrap();
        wal.append(b"two", WalSync::Data).unwrap();
        drop(wal);
        let original = std::fs::read(&path).unwrap();
        assert!(CacheWal::open(&path, WalLimits::default(), |seq, _| {
            if seq == 2 {
                Err(io::Error::other("invalid staged mutation"))
            } else {
                Ok(())
            }
        })
        .is_err());
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }

    #[test]
    fn test_checkpoint_publication_boundaries_and_stable_lock() {
        let temp = Temp::new();
        let path = temp.file();
        let mut wal = CacheWal::open(&path, WalLimits::default(), |_, _| Ok(())).unwrap();
        wal.append(b"old", WalSync::Data).unwrap();
        let original = std::fs::read(&path).unwrap();
        let before = wal.rotate_with(
            1,
            |new| {
                new.append(b"snapshot", WalSync::Buffered)?;
                Ok(())
            },
            |published| {
                if !published {
                    Err(io::Error::other("injected failure before rename"))
                } else {
                    Ok(())
                }
            },
        );
        assert!(before.is_err());
        assert!(!wal.is_poisoned());
        assert_eq!(std::fs::read(&path).unwrap(), original);
        wal.append(b"continued", WalSync::Data).unwrap();
        let after = wal.rotate_with(
            1,
            |new| {
                new.append(b"snapshot", WalSync::Buffered)?;
                Ok(())
            },
            |published| {
                if published {
                    Err(io::Error::other("injected failure after rename"))
                } else {
                    Ok(())
                }
            },
        );
        assert!(after.is_err());
        assert!(wal.is_poisoned());
        assert!(wal.append(b"unsafe", WalSync::Data).is_err());
        assert!(CacheWal::open(&path, WalLimits::default(), |_, _| Ok(())).is_err());
        drop(wal);
        let mut records = Vec::new();
        let mut recovered = CacheWal::open(&path, WalLimits::default(), |seq, payload| {
            records.push((seq, payload.to_vec()));
            Ok(())
        })
        .unwrap();
        assert_eq!(records, [(3, b"snapshot".to_vec())]);
        assert_eq!(recovered.append(b"new", WalSync::Data).unwrap(), 4);
    }

    #[test]
    fn test_checkpoint_required_prefix_is_not_repaired_as_torn_tail() {
        let temp = Temp::new();
        let path = temp.file();
        let mut wal = CacheWal::open(&path, WalLimits::default(), |_, _| Ok(())).unwrap();
        wal.rotate(2, |new| {
            new.append(b"one", WalSync::Buffered)?;
            new.append(b"two", WalSync::Buffered)?;
            Ok(())
        })
        .unwrap();
        assert!(CacheWal::open(&path, WalLimits::default(), |_, _| Ok(())).is_err());
        let snapshot = std::fs::read(&path).unwrap();
        wal.append(b"later", WalSync::Data).unwrap();
        let extended = std::fs::read(&path).unwrap();
        drop(wal);
        for cut in 0..snapshot.len() {
            std::fs::write(&path, &snapshot[..cut]).unwrap();
            assert!(
                CacheWal::open(&path, WalLimits::default(), |_, _| Ok(())).is_err(),
                "cut {cut}"
            );
            assert_eq!(std::fs::metadata(&path).unwrap().len(), cut as u64);
        }
        for cut in snapshot.len()..extended.len() {
            std::fs::write(&path, &extended[..cut]).unwrap();
            let mut seen = Vec::new();
            let recovered = CacheWal::open(&path, WalLimits::default(), |seq, _| {
                seen.push(seq);
                Ok(())
            })
            .unwrap();
            assert_eq!(seen, [1, 2]);
            assert_eq!(recovered.last_sequence(), 2);
            assert_eq!(
                std::fs::metadata(&path).unwrap().len(),
                snapshot.len() as u64
            );
        }
    }

    #[test]
    fn test_checkpoint_count_or_capacity_mismatch_preserves_original() {
        let temp = Temp::new();
        let path = temp.file();
        let limits = WalLimits {
            max_record_bytes: 8,
            max_file_bytes: 120,
        };
        let mut wal = CacheWal::open(&path, limits, |_, _| Ok(())).unwrap();
        wal.append(b"old", WalSync::Data).unwrap();
        let original = std::fs::read(&path).unwrap();
        assert!(wal
            .rotate(2, |new| {
                new.append(b"first", WalSync::Buffered)?;
                Ok(())
            })
            .is_err());
        assert!(wal
            .rotate(2, |new| {
                new.append(b"first", WalSync::Buffered)?;
                new.append(b"second", WalSync::Buffered)?;
                Ok(())
            })
            .is_err());
        assert!(!wal.is_poisoned());
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert_eq!(wal.last_sequence(), 1);
    }

    #[test]
    fn final_sync_failure_poisoning_prevents_further_appends() {
        let temp = Temp::new();
        let path = temp.file();
        let mut wal = CacheWal::open(&path, WalLimits::default(), |_, _| Ok(())).unwrap();
        wal.append(b"buffered", WalSync::Buffered).unwrap();
        wal.synchronize().unwrap();
        assert!(!wal.is_poisoned());
        assert!(wal
            .synchronize_with(|_| Err(io::Error::other("injected sync failure")))
            .is_err());
        assert!(wal.is_poisoned());
        assert!(wal.append(b"unsafe", WalSync::Buffered).is_err());
        assert!(wal.synchronize().is_err());
    }
}
