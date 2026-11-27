//! Single-writer journal framing for the cache persistence layer.
//!
//! Each opaque payload is one atomic logical record. The caller must validate
//! and encode mutations before appending, and must not publish recovered state
//! until open succeeds. No cache mutation path is wired to this module yet.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

const MAGIC: &[u8; 8] = b"NLCWAL02";
const HEADER_BYTES: usize = MAGIC.len();
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
    limits: WalLimits,
    last_sequence: u64,
    valid_len: u64,
    poisoned: bool,
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

impl CacheWal {
    /// Open or exclusively create a journal, stream validated records to a
    /// staging callback, and repair an incomplete final record only. The
    /// caller must discard staging state if any callback or validation fails.
    /// Exclusive advisory locking is held for the entire handle lifetime.
    pub fn open(
        path: impl AsRef<Path>,
        limits: WalLimits,
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
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        File::open(parent)?.sync_all()?;
        let length = file.metadata()?.len();
        if length < HEADER_BYTES as u64 || length > limits.max_file_bytes {
            return Err(invalid("cache WAL file length outside limits"));
        }
        file.seek(SeekFrom::Start(0))?;
        let mut magic = [0; HEADER_BYTES];
        file.read_exact(&mut magic)?;
        if &magic != MAGIC {
            return Err(invalid("unsupported cache WAL magic/version"));
        }

        let mut position = HEADER_BYTES as u64;
        let mut last_sequence = 0u64;
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
        }

        if position != length {
            file.set_len(position)?;
            file.sync_all()?;
        }
        file.seek(SeekFrom::Start(position))?;
        Ok(Self {
            file,
            limits,
            last_sequence,
            valid_len: position,
            poisoned: false,
        })
    }

    pub fn last_sequence(&self) -> u64 {
        self.last_sequence
    }
    pub fn is_poisoned(&self) -> bool {
        self.poisoned
    }

    pub fn append(&mut self, payload: &[u8], sync: WalSync) -> io::Result<u64> {
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
}
