//! Single-writer journal framing for the cache persistence layer.
//!
//! Each opaque payload is one atomic logical record. The caller must validate
//! and encode mutations before appending, and must not publish recovered state
//! until open succeeds. Compaction publishes a complete checkpoint journal atomically.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

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
    file: LockedFile,
    path: PathBuf,
    lock: Arc<LockedFile>,
    limits: WalLimits,
    last_sequence: u64,
    valid_len: u64,
    poisoned: bool,
    #[cfg(all(test, feature = "cache-server"))]
    pub(super) fail_next_append: bool,
    #[cfg(all(test, feature = "cache-server"))]
    pub(super) fail_next_sync: bool,
}

// Explicitly unlock on the final owning guard's drop. A concurrent
// fork/exec or duplicated descriptor can otherwise keep the same open
// file description (and flock) alive after this owner has closed it.
#[derive(Debug)]
struct LockedFile(File);

impl LockedFile {
    fn acquire(file: File) -> io::Result<Self> {
        file.try_lock().map_err(io::Error::other)?;
        Ok(Self(file))
    }
}

impl std::ops::Deref for LockedFile {
    type Target = File;
    fn deref(&self) -> &File {
        &self.0
    }
}

impl std::ops::DerefMut for LockedFile {
    fn deref_mut(&mut self) -> &mut File {
        &mut self.0
    }
}

impl Drop for LockedFile {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

// A cooperating writer requires a single regular inode at each final path
// component. The parent directory remains a trusted deployment boundary.
fn private_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    options
}

fn validate_regular(file: &File) -> io::Result<()> {
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.nlink() != 1 {
        return Err(invalid(
            "cache storage must be a singly linked regular file",
        ));
    }
    Ok(())
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum ClockPublishBoundary {
    Written,
    FileSynced,
    Renamed,
    DirectorySynced,
}

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
        let previous = match private_options().read(true).open(&path) {
            Ok(mut file) => {
                validate_regular(&file)?;
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
        self.publish_with(now, |_| Ok(()))
    }

    fn publish_with(
        &mut self,
        now: u64,
        boundary: impl Fn(ClockPublishBoundary) -> io::Result<()>,
    ) -> io::Result<()> {
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
        let mut file = private_options().write(true).create_new(true).open(&path)?;
        let temp = TemporaryJournal(path);
        file.write_all(&Self::encode(now))?;
        boundary(ClockPublishBoundary::Written)?;
        file.sync_all()?;
        boundary(ClockPublishBoundary::FileSynced)?;
        std::fs::rename(&temp.0, &self.path)?;
        boundary(ClockPublishBoundary::Renamed)?;
        directory.sync_all()?;
        boundary(ClockPublishBoundary::DirectorySynced)?;
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
        let lock = private_options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path)?;
        validate_regular(&lock)?;
        let mut lock = LockedFile::acquire(lock)?;
        let (file, created) = match private_options().read(true).write(true).open(path) {
            Ok(file) => (file, false),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                // Existing durable metadata is evidence of a prior store.
                // Never silently replace a lost WAL with an empty journal.
                if lock.metadata()?.len() != 0 {
                    return Err(invalid("initialized cache journal is missing"));
                }
                match std::fs::symlink_metadata(ClockFloor::path_for(path)) {
                    Ok(_) => {
                        return Err(invalid("cache journal missing but clock metadata exists"))
                    }
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e),
                }
                (
                    private_options()
                        .read(true)
                        .write(true)
                        .create_new(true)
                        .open(path)?,
                    true,
                )
            }
            Err(error) => return Err(error),
        };
        validate_regular(&file)?;
        let mut file = LockedFile::acquire(file)?;
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
            lock: Arc::new(lock),
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
        let file = private_options()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&temp_path)?;
        let temp = TemporaryJournal(temp_path);
        let mut file = LockedFile::acquire(file)?;
        file.write_all(&checkpoint_header(self.last_sequence, entries))?;
        let mut replacement = CacheWal {
            file,
            path: self.path.clone(),
            lock: Arc::clone(&self.lock),
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

    #[test]
    fn journal_and_checkpoint_are_created_with_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = Temp::new();
        let path = tmp.file();
        let mut wal = CacheWal::open(&path, WalLimits::default(), |_, _| Ok(())).unwrap();
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o077;
        assert_eq!(mode(&path), 0);
        assert_eq!(mode(&tmp.0.join("journal.lock")), 0);
        wal.append(b"private", WalSync::Data).unwrap();
        wal.rotate(1, |next| {
            next.append(b"private", WalSync::Buffered)?;
            Ok(())
        })
        .unwrap();
        assert_eq!(mode(&path), 0);
    }

    #[test]
    fn journal_and_lock_reject_symlinks_and_hardlinks_without_mutating_target() {
        use std::os::unix::fs::symlink;
        let tmp = Temp::new();
        let path = tmp.file();
        let target = tmp.0.join("target");
        let mut wal = CacheWal::open(&target, WalLimits::default(), |_, _| Ok(())).unwrap();
        wal.append(b"original", WalSync::Data).unwrap();
        drop(wal);
        let original = std::fs::read(&target).unwrap();
        symlink(&target, &path).unwrap();
        assert!(CacheWal::open(&path, WalLimits::default(), |_, _| Ok(())).is_err());
        std::fs::remove_file(&path).unwrap();
        std::fs::hard_link(&target, &path).unwrap();
        assert!(CacheWal::open(&path, WalLimits::default(), |_, _| Ok(())).is_err());
        std::fs::remove_file(&path).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), original);
        let lock = tmp.0.join("journal.lock");
        if lock.exists() {
            std::fs::remove_file(&lock).unwrap();
        }
        symlink(tmp.0.join("target.lock"), &lock).unwrap();
        assert!(CacheWal::open(&path, WalLimits::default(), |_, _| Ok(())).is_err());
        assert!(!path.exists());
        std::fs::remove_file(&lock).unwrap();
        std::fs::hard_link(tmp.0.join("target.lock"), &lock).unwrap();
        assert!(CacheWal::open(&path, WalLimits::default(), |_, _| Ok(())).is_err());
        assert!(!path.exists());
    }

    #[test]
    fn special_files_are_rejected_without_blocking_or_writing() {
        use std::os::unix::ffi::OsStrExt;
        let tmp = Temp::new();
        let path = tmp.file();
        let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: valid NUL-terminated path in a private test directory.
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
        assert!(CacheWal::open(&path, WalLimits::default(), |_, _| Ok(())).is_err());
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(CacheWal::open(&path, WalLimits::default(), |_, _| Ok(())).is_err());
    }

    #[test]
    fn checkpoint_disk_full_before_publication_preserves_committed_journal() {
        let tmp = Temp::new();
        let path = tmp.file();
        let mut wal = CacheWal::open(&path, WalLimits::default(), |_, _| Ok(())).unwrap();
        wal.append(b"committed", WalSync::Data).unwrap();
        let original = std::fs::read(&path).unwrap();
        let result = wal.rotate(1, |next| {
            next.append_with(b"snapshot", WalSync::Buffered, |file, header, _, _, _| {
                file.write_all(&header[..4])?;
                Err(io::Error::from_raw_os_error(libc::ENOSPC))
            })?;
            Ok(())
        });
        assert_eq!(result.unwrap_err().raw_os_error(), Some(libc::ENOSPC));
        assert!(!wal.is_poisoned());
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert_eq!(wal.append(b"continued", WalSync::Data).unwrap(), 2);
        assert!(!std::fs::read_dir(&tmp.0).unwrap().any(|entry| entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".nulang-checkpoint-")));
    }

    // Re-execute this exact standard test in a private child, then SIGKILL
    // that child at a real I/O boundary. No production crash hook.
    fn kill_at_boundary() -> ! {
        // SAFETY: sends SIGKILL only to the current, isolated test child.
        unsafe { libc::raise(libc::SIGKILL) };
        panic!("SIGKILL unexpectedly returned");
    }

    fn run_crash_child(test: &str, path: &Path, phase: &str) {
        use std::os::unix::process::ExitStatusExt;
        let module = module_path!().split_once("::").unwrap().1;
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &format!("{module}::{test}"), "--nocapture"])
            .env("NULANG_WAL_CRASH_TEST_PATH", path)
            .env("NULANG_WAL_CRASH_TEST_PHASE", phase)
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if std::time::Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("crash child timed out at {phase}");
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        assert_eq!(
            status.signal(),
            Some(libc::SIGKILL),
            "child did not reach {phase}: {status}"
        );
    }

    #[test]
    fn test_wal_process_crash_at_append_boundaries() {
        const PHASES: [&str; 5] = ["header", "payload", "digest", "before-sync", "after-sync"];
        if let Some(path) = std::env::var_os("NULANG_WAL_CRASH_TEST_PATH") {
            let phase = std::env::var("NULANG_WAL_CRASH_TEST_PHASE").unwrap();
            assert!(PHASES.contains(&phase.as_str()));
            let mut wal = CacheWal::open(path, WalLimits::default(), |_, _| Ok(())).unwrap();
            wal.append_with(
                b"pending",
                WalSync::Data,
                |file, header, payload, digest, _| {
                    if phase == "header" {
                        file.write_all(&header[..7])?;
                        kill_at_boundary();
                    }
                    file.write_all(header)?;
                    if phase == "payload" {
                        file.write_all(&payload[..3])?;
                        kill_at_boundary();
                    }
                    file.write_all(payload)?;
                    if phase == "digest" {
                        file.write_all(&digest[..11])?;
                        kill_at_boundary();
                    }
                    file.write_all(digest)?;
                    if phase == "before-sync" {
                        kill_at_boundary();
                    }
                    file.sync_data()?;
                    kill_at_boundary();
                },
            )
            .unwrap();
            unreachable!("child did not stop");
        }
        for phase in PHASES {
            let tmp = Temp::new();
            let path = tmp.file();
            let mut wal = CacheWal::open(&path, WalLimits::default(), |_, _| Ok(())).unwrap();
            wal.append(b"acknowledged", WalSync::Data).unwrap();
            let stable_len = wal.journal_bytes();
            drop(wal);
            run_crash_child("test_wal_process_crash_at_append_boundaries", &path, phase);
            let mut records = Vec::new();
            let mut recovered = CacheWal::open(&path, WalLimits::default(), |seq, payload| {
                records.push((seq, payload.to_vec()));
                Ok(())
            })
            .unwrap();
            assert_eq!(records[0], (1, b"acknowledged".to_vec()), "{phase}");
            match phase {
                "header" | "payload" | "digest" => {
                    assert_eq!(records.len(), 1, "{phase}");
                    assert_eq!(std::fs::metadata(&path).unwrap().len(), stable_len);
                }
                // SIGKILL leaves the kernel/page cache alive; complete
                // unsynced records can legitimately survive.
                "before-sync" | "after-sync" => assert_eq!(
                    records,
                    vec![(1, b"acknowledged".to_vec()), (2, b"pending".to_vec())]
                ),
                _ => unreachable!(),
            }
            assert_eq!(
                recovered.append(b"continued", WalSync::Data).unwrap(),
                records.len() as u64 + 1
            );
        }
    }

    #[test]
    fn test_checkpoint_process_crash_at_publication_boundaries() {
        const PHASES: [&str; 4] = [
            "partial-snapshot",
            "before-rename",
            "after-rename",
            "after-directory-sync",
        ];
        if let Some(path) = std::env::var_os("NULANG_WAL_CRASH_TEST_PATH") {
            let phase = std::env::var("NULANG_WAL_CRASH_TEST_PHASE").unwrap();
            assert!(PHASES.contains(&phase.as_str()));
            let mut wal = CacheWal::open(path, WalLimits::default(), |_, _| Ok(())).unwrap();
            wal.rotate_with(
                1,
                |next| {
                    if phase == "partial-snapshot" {
                        next.append_with(
                            b"snapshot",
                            WalSync::Buffered,
                            |file, header, _, _, _| {
                                file.write_all(&header[..7])?;
                                kill_at_boundary();
                            },
                        )?;
                    } else {
                        next.append(b"snapshot", WalSync::Buffered)?;
                    }
                    Ok(())
                },
                |published| {
                    if (!published && phase == "before-rename")
                        || (published && phase == "after-rename")
                    {
                        kill_at_boundary();
                    }
                    Ok(())
                },
            )
            .unwrap();
            assert_eq!(phase, "after-directory-sync");
            kill_at_boundary();
        }
        for phase in PHASES {
            let tmp = Temp::new();
            let path = tmp.file();
            let mut wal = CacheWal::open(&path, WalLimits::default(), |_, _| Ok(())).unwrap();
            wal.append(b"old", WalSync::Data).unwrap();
            wal.append(b"committed", WalSync::Data).unwrap();
            let original = std::fs::read(&path).unwrap();
            drop(wal);
            run_crash_child(
                "test_checkpoint_process_crash_at_publication_boundaries",
                &path,
                phase,
            );
            let mut records = Vec::new();
            let mut recovered = CacheWal::open(&path, WalLimits::default(), |seq, payload| {
                records.push((seq, payload.to_vec()));
                Ok(())
            })
            .unwrap();
            match phase {
                "partial-snapshot" | "before-rename" => {
                    assert_eq!(std::fs::read(&path).unwrap(), original);
                    assert_eq!(
                        records,
                        vec![(1, b"old".to_vec()), (2, b"committed".to_vec())]
                    );
                    assert_eq!(recovered.append(b"continued", WalSync::Data).unwrap(), 3);
                }
                "after-rename" | "after-directory-sync" => {
                    assert_eq!(records, vec![(3, b"snapshot".to_vec())]);
                    assert_eq!(recovered.append(b"continued", WalSync::Data).unwrap(), 4);
                }
                _ => unreachable!(),
            }
            assert!(CacheWal::open(&path, WalLimits::default(), |_, _| Ok(())).is_err());
        }
    }

    #[test]
    fn test_clock_process_crash_at_publication_boundaries() {
        const PHASES: [(&str, ClockPublishBoundary); 4] = [
            ("written", ClockPublishBoundary::Written),
            ("file-synced", ClockPublishBoundary::FileSynced),
            ("renamed", ClockPublishBoundary::Renamed),
            ("directory-synced", ClockPublishBoundary::DirectorySynced),
        ];
        if let Some(path) = std::env::var_os("NULANG_WAL_CRASH_TEST_PATH") {
            let phase = std::env::var("NULANG_WAL_CRASH_TEST_PHASE").unwrap();
            let selected = PHASES.iter().find(|(name, _)| *name == phase).unwrap().1;
            let mut clock = None;
            let _wal = CacheWal::open_prepared(
                path,
                WalLimits::default(),
                |path, lock| {
                    clock = Some(ClockFloor::open(path, lock, 1000)?);
                    Ok(())
                },
                |_, _| Ok(()),
            )
            .unwrap();
            clock
                .unwrap()
                .publish_with(2000, |point| {
                    if point == selected {
                        kill_at_boundary();
                    }
                    Ok(())
                })
                .unwrap();
            unreachable!("child did not stop");
        }
        for (phase, point) in PHASES {
            let tmp = Temp::new();
            let path = tmp.file();
            let mut wal = CacheWal::open_prepared(
                &path,
                WalLimits::default(),
                |path, lock| {
                    ClockFloor::open(path, lock, 1000)?;
                    Ok(())
                },
                |_, _| Ok(()),
            )
            .unwrap();
            wal.append(b"committed", WalSync::Data).unwrap();
            drop(wal);
            run_crash_child(
                "test_clock_process_crash_at_publication_boundaries",
                &path,
                phase,
            );
            let expected_floor = match point {
                ClockPublishBoundary::Written | ClockPublishBoundary::FileSynced => 1000,
                ClockPublishBoundary::Renamed | ClockPublishBoundary::DirectorySynced => 2000,
            };
            let persisted = std::fs::read(ClockFloor::path_for(&path)).unwrap();
            assert_eq!(persisted, ClockFloor::encode(expected_floor), "{phase}");
            assert!(CacheWal::open_prepared(
                &path,
                WalLimits::default(),
                |path, lock| {
                    ClockFloor::open(path, lock, expected_floor - 1)?;
                    Ok(())
                },
                |_, _| panic!("rollback should not reach replay")
            )
            .is_err());
            assert_eq!(
                std::fs::read(ClockFloor::path_for(&path)).unwrap(),
                persisted
            );
            let mut records = Vec::new();
            let mut recovered = CacheWal::open_prepared(
                &path,
                WalLimits::default(),
                |path, lock| {
                    ClockFloor::open(path, lock, expected_floor)?;
                    Ok(())
                },
                |seq, payload| {
                    records.push((seq, payload.to_vec()));
                    Ok(())
                },
            )
            .unwrap();
            assert_eq!(records, vec![(1, b"committed".to_vec())]);
            assert_eq!(recovered.append(b"continued", WalSync::Data).unwrap(), 2);
        }
    }

    #[test]
    fn test_wal_releases_locks_even_if_an_unrelated_fork_retains_file_descriptors() {
        let tmp = Temp::new();
        let path = tmp.file();
        let mut wal = CacheWal::open(&path, WalLimits::default(), |_, _| Ok(())).unwrap();
        // Duplicates share the open file descriptions, just as inherited
        // descriptors do in the interval between fork and exec.
        let inherited_file = wal.file.try_clone().unwrap();
        let inherited_lock = wal.lock.try_clone().unwrap();
        assert!(CacheWal::open(&path, WalLimits::default(), |_, _| Ok(())).is_err());
        wal.rotate_with(
            1,
            |next| {
                next.append(b"snapshot", WalSync::Buffered)?;
                Ok(())
            },
            |_| {
                assert!(CacheWal::open(&path, WalLimits::default(), |_, _| Ok(())).is_err());
                Ok(())
            },
        )
        .unwrap();
        let inherited_replacement = wal.file.try_clone().unwrap();
        assert!(CacheWal::open(&path, WalLimits::default(), |_, _| Ok(())).is_err());
        drop(wal);
        let mut next = CacheWal::open(&path, WalLimits::default(), |_, _| Ok(())).unwrap();
        assert_eq!(next.append(b"new writer", WalSync::Data).unwrap(), 2);
        drop(inherited_file);
        drop(inherited_replacement);
        drop(inherited_lock);
    }

    /// Enumerate independent 512-byte persistence choices for a two-record
    /// unacknowledged suffix. File length is independently persisted at the
    /// full length, so missing interior/tail sectors are holes, not EOF.
    #[test]
    fn test_storage_model_reordered_and_torn_sector_matrix() {
        const SECTOR: usize = 512;
        let tmp = Temp::new();
        let path = tmp.file();
        let mut wal = CacheWal::open(&path, WalLimits::default(), |_, _| Ok(())).unwrap();
        // Align the stable prefix to a sector: 8-byte magic + 48-byte frame.
        let anchor = vec![b'a'; SECTOR - HEADER_BYTES - RECORD_HEADER_BYTES - CHECKSUM_BYTES];
        wal.append(&anchor, WalSync::Data).unwrap();
        let stable = std::fs::read(&path).unwrap();
        assert_eq!(stable.len(), SECTOR);
        let a = vec![b'b'; SECTOR * 2 - RECORD_HEADER_BYTES - CHECKSUM_BYTES];
        let b = vec![b'c'; SECTOR * 2 - RECORD_HEADER_BYTES - CHECKSUM_BYTES];
        wal.append(&a, WalSync::Buffered).unwrap();
        wal.append(&b, WalSync::Buffered).unwrap();
        drop(wal);
        let complete = std::fs::read(&path).unwrap();
        assert_eq!(complete.len(), 5 * SECTOR);
        // All 16 subsets of the four suffix sectors, crossed with four
        // representative prefix lengths inside each missing sector.
        let mut cases = 0;
        for mask in 0..16u8 {
            for torn in [0, 1, 255, 511] {
                let mut image = stable.clone();
                image.resize(complete.len(), 0);
                for index in 0..4 {
                    let start = (index + 1) * SECTOR;
                    let n = if mask & (1 << index) != 0 {
                        SECTOR
                    } else {
                        torn
                    };
                    image[start..start + n].copy_from_slice(&complete[start..start + n]);
                }
                std::fs::write(&path, &image).unwrap();
                let mut seen = Vec::new();
                let outcome = CacheWal::open(&path, WalLimits::default(), |seq, payload| {
                    seen.push((seq, payload.to_vec()));
                    Ok(())
                });
                if image == complete {
                    assert!(outcome.is_ok(), "complete suffix rejected");
                    assert_eq!(seen, [(1, anchor.clone()), (2, a.clone()), (3, b.clone())]);
                } else {
                    assert!(
                        outcome.is_err(),
                        "mask {mask:04b}, tear {torn} accepted corrupt bytes"
                    );
                    assert_eq!(
                        std::fs::read(&path).unwrap(),
                        image,
                        "corruption was rewritten"
                    );
                }
                cases += 1;
            }
        }
        // Persist the logical EOF independently: real prefix truncations
        // retain exactly the complete record prefix, never a partial record.
        for cut in (SECTOR..=complete.len())
            .step_by(127)
            .chain([3 * SECTOR, 5 * SECTOR])
        {
            std::fs::write(&path, &complete[..cut]).unwrap();
            let mut seen = Vec::new();
            let recovered = CacheWal::open(&path, WalLimits::default(), |seq, payload| {
                seen.push((seq, payload.to_vec()));
                Ok(())
            })
            .unwrap();
            let n = if cut == complete.len() {
                3
            } else if cut >= 3 * SECTOR {
                2
            } else {
                1
            };
            assert_eq!(recovered.last_sequence(), n);
            assert_eq!(
                seen,
                [(1, anchor.clone()), (2, a.clone()), (3, b.clone())][..n as usize]
            );
            let expected_len = SECTOR + (n as usize - 1) * 2 * SECTOR;
            assert_eq!(std::fs::metadata(&path).unwrap().len(), expected_len as u64);
            cases += 1;
        }
        // Fully checksummed records reordered or duplicated must still
        // fail the contiguous sequence check.
        for order in [[1, 0], [0, 0], [1, 1]] {
            let mut image = stable.clone();
            for index in order {
                let start = SECTOR + index * 2 * SECTOR;
                image.extend_from_slice(&complete[start..start + 2 * SECTOR]);
            }
            std::fs::write(&path, &image).unwrap();
            assert!(CacheWal::open(&path, WalLimits::default(), |_, _| Ok(())).is_err());
            assert_eq!(std::fs::read(&path).unwrap(), image);
            cases += 1;
        }
        println!("storage model WAL: {cases} deterministic images");
    }

    #[test]
    fn test_storage_model_checkpoint_4k_sector_faults() {
        const SECTOR: usize = 4096;
        let tmp = Temp::new();
        let path = tmp.file();
        let mut wal = CacheWal::open(&path, WalLimits::default(), |_, _| Ok(())).unwrap();
        // Checkpoint header plus first frame occupies exactly one sector.
        let first =
            vec![b'a'; SECTOR - CHECKPOINT_HEADER_BYTES - RECORD_HEADER_BYTES - CHECKSUM_BYTES];
        let second = vec![b'b'; SECTOR - RECORD_HEADER_BYTES - CHECKSUM_BYTES];
        let third = vec![b'c'; SECTOR - RECORD_HEADER_BYTES - CHECKSUM_BYTES];
        wal.append(b"old journal", WalSync::Data).unwrap();
        wal.rotate(3, |next| {
            for payload in [&first, &second, &third] {
                next.append(payload, WalSync::Buffered)?;
            }
            Ok(())
        })
        .unwrap();
        drop(wal);
        let snapshot = std::fs::read(&path).unwrap();
        assert_eq!(snapshot.len(), SECTOR * 3);
        let mut cases = 0;
        // Sector omissions, stale-neighbor replacements, and deterministic
        // torn-sector suffixes in the required snapshot prefix.
        for sector in 0..3 {
            let start = sector * SECTOR;
            let end = start + SECTOR;
            for cut in [0, 1, 512, 2048, 4095] {
                for replacement in [None, Some((sector + 1) % 3)] {
                    let mut image = snapshot.clone();
                    let suffix = &mut image[start + cut..end];
                    match replacement {
                        None => suffix.fill(0),
                        Some(source) => suffix.copy_from_slice(
                            &snapshot[source * SECTOR + cut..(source + 1) * SECTOR],
                        ),
                    }
                    assert_ne!(image, snapshot);
                    std::fs::write(&path, &image).unwrap();
                    assert!(
                        CacheWal::open(&path, WalLimits::default(), |_, _| Ok(())).is_err(),
                        "sector {sector} cut {cut} replacement {replacement:?}"
                    );
                    assert_eq!(std::fs::read(&path).unwrap(), image);
                    cases += 1;
                }
            }
        }
        // Losing entire suffix sectors is not recoverable for a required
        // checkpoint prefix, even with a valid checksummed header.
        for cut in [0, SECTOR, 2 * SECTOR] {
            std::fs::write(&path, &snapshot[..cut]).unwrap();
            assert!(CacheWal::open(&path, WalLimits::default(), |_, _| Ok(())).is_err());
            assert_eq!(std::fs::read(&path).unwrap(), snapshot[..cut]);
            cases += 1;
        }
        std::fs::write(&path, &snapshot).unwrap();
        let mut seen = Vec::new();
        let mut recovered = CacheWal::open(&path, WalLimits::default(), |seq, payload| {
            seen.push((seq, payload.to_vec()));
            Ok(())
        })
        .unwrap();
        assert_eq!(seen, [(2, first), (3, second), (4, third)]);
        assert_eq!(recovered.append(b"continued", WalSync::Data).unwrap(), 5);
        cases += 1;
        println!("storage model 4K checkpoint: {cases} deterministic images");
    }
}
