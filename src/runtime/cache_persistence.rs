//! Durable snapshot and write-ahead-log substrate for the RESP cache.
//!
//! The shard-local CacheStore remains an in-memory data plane. This module owns
//! durable bytes, checksums, wall-clock TTL translation, and crash recovery.
//! Process-relative CacheStore timestamps are never written to disk.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use super::cache::{
    CacheConfig, CacheEvictionPolicy, CacheSnapshotEntry, CacheSnapshotValue, CacheStore,
    CacheWriteError,
};

const SNAPSHOT_MAGIC: &[u8; 8] = b"NLCACH01";
const WAL_MAGIC: &[u8; 8] = b"NLCWAL01";
const CACHE_PERSISTENCE_VERSION: u16 = 1;
const CHECKSUM_BYTES: usize = 32;
const WAL_HEADER_BYTES: usize = 8 + 2 + 8;
const MAX_WAL_RECORD_BYTES: usize = 128 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheWalSync {
    Buffered,
    Data,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheWalMutation {
    SetBytes {
        key: Vec<u8>,
        value: Vec<u8>,
        expires_unix_ms: Option<u64>,
    },
    SetInteger {
        key: Vec<u8>,
        value: i64,
        expires_unix_ms: Option<u64>,
    },
    Delete {
        key: Vec<u8>,
    },
    ExpireAt {
        key: Vec<u8>,
        expires_unix_ms: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheWalRecord {
    pub sequence: u64,
    pub mutation: CacheWalMutation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheRecoveryReport {
    pub snapshot_sequence: u64,
    pub wal_base_sequence: u64,
    pub wal_last_sequence: u64,
    pub replayed_records: usize,
}

#[derive(Debug)]
pub struct CacheWal {
    path: PathBuf,
    file: File,
    base_sequence: u64,
    last_sequence: u64,
}

impl CacheWal {
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        if !path.exists() {
            write_new_wal(&path, 0)?;
        }

        let bytes = fs::read(&path)?;
        let scan = scan_wal(&bytes)?;

        let mut file = OpenOptions::new().read(true).write(true).open(&path)?;
        if scan.valid_len < bytes.len() {
            file.set_len(scan.valid_len as u64)?;
            file.sync_data()?;
        }
        file.seek(SeekFrom::End(0))?;

        Ok(Self {
            path,
            file,
            base_sequence: scan.base_sequence,
            last_sequence: scan.last_sequence,
        })
    }

    /// Create a fresh WAL whose first future record follows base_sequence.
    ///
    /// This is the rotation primitive used after a snapshot has been made
    /// durable. The caller must only replace an older WAL once that snapshot
    /// is itself safely published.
    pub fn create_after(path: impl AsRef<Path>, base_sequence: u64) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        write_new_wal(&path, base_sequence)?;
        Self::open(path)
    }

    pub fn base_sequence(&self) -> u64 {
        self.base_sequence
    }

    pub fn last_sequence(&self) -> u64 {
        self.last_sequence
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn append(&mut self, mutation: &CacheWalMutation, sync: CacheWalSync) -> io::Result<u64> {
        let sequence = self
            .last_sequence
            .checked_add(1)
            .ok_or_else(|| invalid_data("cache WAL sequence overflow"))?;
        let payload = encode_mutation(mutation)?;
        if payload.len() > MAX_WAL_RECORD_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "cache WAL record exceeds maximum size",
            ));
        }

        let payload_len = u32::try_from(payload.len()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "cache WAL record length exceeds u32",
            )
        })?;
        let mut checksum_input = Vec::with_capacity(8 + payload.len());
        checksum_input.extend_from_slice(&sequence.to_le_bytes());
        checksum_input.extend_from_slice(&payload);
        let checksum = blake3::hash(&checksum_input);

        self.file.write_all(&payload_len.to_le_bytes())?;
        self.file
            .write_all(&(payload_len ^ u32::MAX).to_le_bytes())?;
        self.file.write_all(&sequence.to_le_bytes())?;
        self.file.write_all(&payload)?;
        self.file.write_all(checksum.as_bytes())?;

        if sync == CacheWalSync::Data {
            self.file.sync_data()?;
        }
        self.last_sequence = sequence;
        Ok(sequence)
    }
}

pub fn write_cache_snapshot(
    path: impl AsRef<Path>,
    store: &CacheStore,
    wal_sequence: u64,
    store_now_ms: u64,
    wall_now_ms: u64,
) -> io::Result<()> {
    let entries = store.snapshot_entries(store_now_ms);
    let mut bytes = Vec::new();
    bytes.extend_from_slice(SNAPSHOT_MAGIC);
    bytes.extend_from_slice(&CACHE_PERSISTENCE_VERSION.to_le_bytes());
    bytes.extend_from_slice(&wal_sequence.to_le_bytes());
    bytes.extend_from_slice(&wall_now_ms.to_le_bytes());
    bytes.extend_from_slice(&(entries.len() as u64).to_le_bytes());

    for entry in entries {
        encode_bytes(&mut bytes, &entry.key)?;
        match entry.value {
            CacheSnapshotValue::Bytes(value) => {
                bytes.push(0);
                encode_bytes(&mut bytes, &value)?;
            }
            CacheSnapshotValue::Integer(value) => {
                bytes.push(1);
                bytes.extend_from_slice(&value.to_le_bytes());
            }
        }

        match entry.remaining_ttl_ms {
            None => bytes.push(0),
            Some(remaining) => {
                bytes.push(1);
                bytes.extend_from_slice(&wall_now_ms.saturating_add(remaining).to_le_bytes());
            }
        }
    }

    let checksum = blake3::hash(&bytes);
    bytes.extend_from_slice(checksum.as_bytes());
    atomic_write(path.as_ref(), &bytes)
}

pub fn recover_cache(
    snapshot_path: impl AsRef<Path>,
    wal_path: impl AsRef<Path>,
    config: CacheConfig,
    eviction_policy: CacheEvictionPolicy,
    store_now_ms: u64,
    wall_now_ms: u64,
) -> io::Result<(CacheStore, CacheRecoveryReport)> {
    let (snapshot_sequence, entries) = load_cache_snapshot(snapshot_path.as_ref(), wall_now_ms)?;
    let mut store = CacheStore::from_snapshot(config, eviction_policy, &entries, store_now_ms)
        .map_err(cache_write_error)?;

    let wal_bytes = match fs::read(wal_path.as_ref()) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok((
                store,
                CacheRecoveryReport {
                    snapshot_sequence,
                    wal_base_sequence: 0,
                    wal_last_sequence: 0,
                    replayed_records: 0,
                },
            ));
        }
        Err(error) => return Err(error),
    };
    let scan = scan_wal(&wal_bytes)?;

    if scan.base_sequence > snapshot_sequence {
        return Err(invalid_data(
            "cache snapshot is older than the WAL base sequence; required history is missing",
        ));
    }

    let mut replayed_records = 0usize;
    for record in scan.records {
        if record.sequence <= snapshot_sequence {
            continue;
        }
        apply_wal_mutation(&mut store, &record.mutation, store_now_ms, wall_now_ms)?;
        replayed_records += 1;
    }

    Ok((
        store,
        CacheRecoveryReport {
            snapshot_sequence,
            wal_base_sequence: scan.base_sequence,
            wal_last_sequence: scan.last_sequence,
            replayed_records,
        },
    ))
}

pub fn apply_wal_mutation(
    store: &mut CacheStore,
    mutation: &CacheWalMutation,
    store_now_ms: u64,
    wall_now_ms: u64,
) -> io::Result<()> {
    match mutation {
        CacheWalMutation::SetBytes {
            key,
            value,
            expires_unix_ms,
        } => {
            let Some(ttl) = recovered_ttl(*expires_unix_ms, wall_now_ms) else {
                store.delete(key);
                return Ok(());
            };
            store
                .try_set_bytes(key, value, ttl, store_now_ms)
                .map_err(cache_write_error)
        }
        CacheWalMutation::SetInteger {
            key,
            value,
            expires_unix_ms,
        } => {
            let Some(ttl) = recovered_ttl(*expires_unix_ms, wall_now_ms) else {
                store.delete(key);
                return Ok(());
            };
            store
                .try_set_integer(key, *value, ttl, store_now_ms)
                .map_err(cache_write_error)
        }
        CacheWalMutation::Delete { key } => {
            store.delete(key);
            Ok(())
        }
        CacheWalMutation::ExpireAt {
            key,
            expires_unix_ms,
        } => {
            if *expires_unix_ms <= wall_now_ms {
                store.delete(key);
            } else {
                store.expire_ms(key, expires_unix_ms - wall_now_ms, store_now_ms);
            }
            Ok(())
        }
    }
}

fn recovered_ttl(expires_unix_ms: Option<u64>, wall_now_ms: u64) -> Option<Option<u64>> {
    match expires_unix_ms {
        None => Some(None),
        Some(deadline) if deadline <= wall_now_ms => None,
        Some(deadline) => Some(Some(deadline - wall_now_ms)),
    }
}

fn load_cache_snapshot(
    path: &Path,
    wall_now_ms: u64,
) -> io::Result<(u64, Vec<CacheSnapshotEntry>)> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok((0, Vec::new())),
        Err(error) => return Err(error),
    };
    if bytes.len() < 8 + 2 + 8 + 8 + 8 + CHECKSUM_BYTES {
        return Err(invalid_data("cache snapshot is truncated"));
    }

    let body_len = bytes.len() - CHECKSUM_BYTES;
    let (body, stored_checksum) = bytes.split_at(body_len);
    let actual_checksum = blake3::hash(body);
    if actual_checksum.as_bytes() != stored_checksum {
        return Err(invalid_data("cache snapshot checksum mismatch"));
    }

    let mut decoder = Decoder::new(body);
    if decoder.take(8)? != SNAPSHOT_MAGIC {
        return Err(invalid_data("invalid cache snapshot magic"));
    }
    let version = decoder.u16()?;
    if version != CACHE_PERSISTENCE_VERSION {
        return Err(invalid_data("unsupported cache snapshot version"));
    }
    let wal_sequence = decoder.u64()?;
    let _created_unix_ms = decoder.u64()?;
    let entry_count = decoder.u64()?;

    let capacity = usize::try_from(entry_count)
        .map_err(|_| invalid_data("cache snapshot entry count exceeds platform limits"))?;
    let mut entries = Vec::with_capacity(capacity);

    for _ in 0..entry_count {
        let key = decoder.bytes()?;
        let value = match decoder.u8()? {
            0 => CacheSnapshotValue::Bytes(decoder.bytes()?),
            1 => CacheSnapshotValue::Integer(decoder.i64()?),
            _ => return Err(invalid_data("invalid cache snapshot value tag")),
        };
        let remaining_ttl_ms = match decoder.u8()? {
            0 => None,
            1 => {
                let expires_unix_ms = decoder.u64()?;
                if expires_unix_ms <= wall_now_ms {
                    continue;
                }
                Some(expires_unix_ms - wall_now_ms)
            }
            _ => return Err(invalid_data("invalid cache snapshot TTL tag")),
        };
        entries.push(CacheSnapshotEntry {
            key,
            value,
            remaining_ttl_ms,
        });
    }
    decoder.finish()?;
    Ok((wal_sequence, entries))
}

struct WalScan {
    base_sequence: u64,
    last_sequence: u64,
    valid_len: usize,
    records: Vec<CacheWalRecord>,
}

fn scan_wal(bytes: &[u8]) -> io::Result<WalScan> {
    if bytes.len() < WAL_HEADER_BYTES {
        return Err(invalid_data("cache WAL header is truncated"));
    }
    if &bytes[..8] != WAL_MAGIC {
        return Err(invalid_data("invalid cache WAL magic"));
    }
    let version = u16::from_le_bytes([bytes[8], bytes[9]]);
    if version != CACHE_PERSISTENCE_VERSION {
        return Err(invalid_data("unsupported cache WAL version"));
    }
    let base_sequence = u64::from_le_bytes(
        bytes[10..18]
            .try_into()
            .expect("fixed cache WAL base-sequence slice"),
    );

    let mut position = WAL_HEADER_BYTES;
    let mut last_sequence = base_sequence;
    let mut expected_sequence = base_sequence.saturating_add(1);
    let mut records = Vec::new();

    while position < bytes.len() {
        let record_start = position;
        if bytes.len() - position < 8 {
            return Ok(WalScan {
                base_sequence,
                last_sequence,
                valid_len: record_start,
                records,
            });
        }

        let payload_len_raw = u32::from_le_bytes(
            bytes[position..position + 4]
                .try_into()
                .expect("fixed cache WAL length slice"),
        );
        let payload_len_check = u32::from_le_bytes(
            bytes[position + 4..position + 8]
                .try_into()
                .expect("fixed cache WAL complemented-length slice"),
        );
        position += 8;
        if payload_len_check != (payload_len_raw ^ u32::MAX) {
            return Err(invalid_data("cache WAL record length header is corrupt"));
        }
        let payload_len = payload_len_raw as usize;

        let record_tail = 8usize
            .checked_add(payload_len)
            .and_then(|value| value.checked_add(CHECKSUM_BYTES))
            .ok_or_else(|| invalid_data("cache WAL record length overflow"))?;
        if bytes.len() - position < record_tail {
            return Ok(WalScan {
                base_sequence,
                last_sequence,
                valid_len: record_start,
                records,
            });
        }
        if payload_len > MAX_WAL_RECORD_BYTES {
            return Err(invalid_data("cache WAL record exceeds maximum size"));
        }

        let sequence = u64::from_le_bytes(
            bytes[position..position + 8]
                .try_into()
                .expect("fixed cache WAL sequence slice"),
        );
        position += 8;
        if sequence != expected_sequence {
            return Err(invalid_data("cache WAL sequence is not contiguous"));
        }

        let payload = &bytes[position..position + payload_len];
        position += payload_len;
        let stored_checksum = &bytes[position..position + CHECKSUM_BYTES];
        position += CHECKSUM_BYTES;

        let mut checksum_input = Vec::with_capacity(8 + payload.len());
        checksum_input.extend_from_slice(&sequence.to_le_bytes());
        checksum_input.extend_from_slice(payload);
        let actual_checksum = blake3::hash(&checksum_input);
        if actual_checksum.as_bytes() != stored_checksum {
            return Err(invalid_data("cache WAL record checksum mismatch"));
        }

        records.push(CacheWalRecord {
            sequence,
            mutation: decode_mutation(payload)?,
        });
        last_sequence = sequence;
        expected_sequence = sequence
            .checked_add(1)
            .ok_or_else(|| invalid_data("cache WAL sequence overflow"))?;
    }

    Ok(WalScan {
        base_sequence,
        last_sequence,
        valid_len: position,
        records,
    })
}

fn encode_mutation(mutation: &CacheWalMutation) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    match mutation {
        CacheWalMutation::SetBytes {
            key,
            value,
            expires_unix_ms,
        } => {
            bytes.push(0);
            encode_bytes(&mut bytes, key)?;
            encode_bytes(&mut bytes, value)?;
            encode_optional_u64(&mut bytes, *expires_unix_ms);
        }
        CacheWalMutation::SetInteger {
            key,
            value,
            expires_unix_ms,
        } => {
            bytes.push(1);
            encode_bytes(&mut bytes, key)?;
            bytes.extend_from_slice(&value.to_le_bytes());
            encode_optional_u64(&mut bytes, *expires_unix_ms);
        }
        CacheWalMutation::Delete { key } => {
            bytes.push(2);
            encode_bytes(&mut bytes, key)?;
        }
        CacheWalMutation::ExpireAt {
            key,
            expires_unix_ms,
        } => {
            bytes.push(3);
            encode_bytes(&mut bytes, key)?;
            bytes.extend_from_slice(&expires_unix_ms.to_le_bytes());
        }
    }
    Ok(bytes)
}

fn decode_mutation(bytes: &[u8]) -> io::Result<CacheWalMutation> {
    let mut decoder = Decoder::new(bytes);
    let mutation = match decoder.u8()? {
        0 => CacheWalMutation::SetBytes {
            key: decoder.bytes()?,
            value: decoder.bytes()?,
            expires_unix_ms: decoder.optional_u64()?,
        },
        1 => CacheWalMutation::SetInteger {
            key: decoder.bytes()?,
            value: decoder.i64()?,
            expires_unix_ms: decoder.optional_u64()?,
        },
        2 => CacheWalMutation::Delete {
            key: decoder.bytes()?,
        },
        3 => CacheWalMutation::ExpireAt {
            key: decoder.bytes()?,
            expires_unix_ms: decoder.u64()?,
        },
        _ => return Err(invalid_data("invalid cache WAL mutation tag")),
    };
    decoder.finish()?;
    Ok(mutation)
}

fn write_new_wal(path: &Path, base_sequence: u64) -> io::Result<()> {
    let mut bytes = Vec::with_capacity(WAL_HEADER_BYTES);
    bytes.extend_from_slice(WAL_MAGIC);
    bytes.extend_from_slice(&CACHE_PERSISTENCE_VERSION.to_le_bytes());
    bytes.extend_from_slice(&base_sequence.to_le_bytes());
    atomic_write(path, &bytes)
}

fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;

    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("cache-state");
    let temp = parent.join(format!(".{file_name}.tmp-{}", std::process::id()));

    let result = (|| {
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temp, path)?;
        #[cfg(unix)]
        File::open(parent)?.sync_all()?;
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn encode_bytes(out: &mut Vec<u8>, value: &[u8]) -> io::Result<()> {
    let len = u32::try_from(value.len()).map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidInput, "cache byte string exceeds u32")
    })?;
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(value);
    Ok(())
}

fn encode_optional_u64(out: &mut Vec<u8>, value: Option<u64>) {
    match value {
        None => out.push(0),
        Some(value) => {
            out.push(1);
            out.extend_from_slice(&value.to_le_bytes());
        }
    }
}

struct Decoder<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Decoder<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    fn take(&mut self, len: usize) -> io::Result<&'a [u8]> {
        let end = self
            .position
            .checked_add(len)
            .ok_or_else(|| invalid_data("cache persistence offset overflow"))?;
        let value = self
            .bytes
            .get(self.position..end)
            .ok_or_else(|| invalid_data("cache persistence record is truncated"))?;
        self.position = end;
        Ok(value)
    }

    fn u8(&mut self) -> io::Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> io::Result<u16> {
        Ok(u16::from_le_bytes(
            self.take(2)?.try_into().expect("fixed u16 slice"),
        ))
    }

    fn u32(&mut self) -> io::Result<u32> {
        Ok(u32::from_le_bytes(
            self.take(4)?.try_into().expect("fixed u32 slice"),
        ))
    }

    fn u64(&mut self) -> io::Result<u64> {
        Ok(u64::from_le_bytes(
            self.take(8)?.try_into().expect("fixed u64 slice"),
        ))
    }

    fn i64(&mut self) -> io::Result<i64> {
        Ok(i64::from_le_bytes(
            self.take(8)?.try_into().expect("fixed i64 slice"),
        ))
    }

    fn bytes(&mut self) -> io::Result<Vec<u8>> {
        let len = self.u32()? as usize;
        Ok(self.take(len)?.to_vec())
    }

    fn optional_u64(&mut self) -> io::Result<Option<u64>> {
        match self.u8()? {
            0 => Ok(None),
            1 => Ok(Some(self.u64()?)),
            _ => Err(invalid_data("invalid cache optional-u64 tag")),
        }
    }

    fn finish(self) -> io::Result<()> {
        if self.position == self.bytes.len() {
            Ok(())
        } else {
            Err(invalid_data("cache persistence record has trailing bytes"))
        }
    }
}

fn cache_write_error(error: CacheWriteError) -> io::Error {
    invalid_data(match error {
        CacheWriteError::KeyTooLarge => "recovered cache key exceeds configured maximum",
        CacheWriteError::ValueTooLarge => "recovered cache value exceeds configured maximum",
        CacheWriteError::EntryLimitReached => "recovered cache exceeds configured entry limit",
        CacheWriteError::ArenaLimitReached => "recovered cache exceeds configured arena limit",
    })
}

fn invalid_data(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::super::cache::{CacheTtl, CacheValueView};
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEST_ID: AtomicU64 = AtomicU64::new(1);

    fn test_path(name: &str) -> PathBuf {
        let id = NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("nulang-cache-{name}-{}-{id}", std::process::id()))
    }

    #[test]
    fn snapshot_round_trip_translates_ttl_across_restart() {
        let snapshot = test_path("snapshot");
        let wal = test_path("missing-wal");
        let mut store = CacheStore::new();
        store.set_bytes(b"persistent", b"value", None, 100);
        store.set_integer(b"ttl", 42, Some(10_000), 100);

        write_cache_snapshot(&snapshot, &store, 0, 1_100, 1_000_000).unwrap();
        let (mut recovered, report) = recover_cache(
            &snapshot,
            &wal,
            CacheConfig::default(),
            CacheEvictionPolicy::S3Fifo,
            50,
            1_002_000,
        )
        .unwrap();

        assert_eq!(report.snapshot_sequence, 0);
        assert_eq!(
            recovered.get(b"persistent", 50),
            Some(CacheValueView::Bytes(b"value"))
        );
        assert_eq!(recovered.get(b"ttl", 50), Some(CacheValueView::Integer(42)));
        assert_eq!(recovered.ttl(b"ttl", 50), CacheTtl::RemainingMs(7_000));

        let _ = fs::remove_file(snapshot);
    }

    #[test]
    fn wal_replays_only_records_after_snapshot_sequence() {
        let snapshot = test_path("snapshot-wal");
        let wal_path = test_path("wal");
        let mut wal = CacheWal::create_after(&wal_path, 0).unwrap();
        let mut store = CacheStore::new();

        let set_a = CacheWalMutation::SetBytes {
            key: b"a".to_vec(),
            value: b"one".to_vec(),
            expires_unix_ms: None,
        };
        let seq1 = wal.append(&set_a, CacheWalSync::Data).unwrap();
        apply_wal_mutation(&mut store, &set_a, 0, 1_000).unwrap();
        write_cache_snapshot(&snapshot, &store, seq1, 0, 1_000).unwrap();

        let set_b = CacheWalMutation::SetInteger {
            key: b"b".to_vec(),
            value: 7,
            expires_unix_ms: Some(6_000),
        };
        wal.append(&set_b, CacheWalSync::Data).unwrap();
        wal.append(
            &CacheWalMutation::Delete { key: b"a".to_vec() },
            CacheWalSync::Data,
        )
        .unwrap();
        drop(wal);

        let (mut recovered, report) = recover_cache(
            &snapshot,
            &wal_path,
            CacheConfig::default(),
            CacheEvictionPolicy::S3Fifo,
            100,
            2_000,
        )
        .unwrap();

        assert_eq!(report.snapshot_sequence, 1);
        assert_eq!(report.wal_last_sequence, 3);
        assert_eq!(report.replayed_records, 2);
        assert_eq!(recovered.get(b"a", 100), None);
        assert_eq!(recovered.get(b"b", 100), Some(CacheValueView::Integer(7)));
        assert_eq!(recovered.ttl(b"b", 100), CacheTtl::RemainingMs(4_000));

        let _ = fs::remove_file(snapshot);
        let _ = fs::remove_file(wal_path);
    }

    #[test]
    fn wal_open_truncates_torn_tail_without_losing_complete_record() {
        let wal_path = test_path("torn");
        let mut wal = CacheWal::create_after(&wal_path, 0).unwrap();
        wal.append(
            &CacheWalMutation::SetInteger {
                key: b"k".to_vec(),
                value: 1,
                expires_unix_ms: None,
            },
            CacheWalSync::Data,
        )
        .unwrap();
        drop(wal);

        let complete_len = fs::metadata(&wal_path).unwrap().len();
        let mut file = OpenOptions::new().append(true).open(&wal_path).unwrap();
        file.write_all(&[0x10, 0x00, 0x00]).unwrap();
        file.sync_data().unwrap();
        drop(file);

        let reopened = CacheWal::open(&wal_path).unwrap();
        assert_eq!(reopened.last_sequence(), 1);
        assert_eq!(fs::metadata(&wal_path).unwrap().len(), complete_len);

        let _ = fs::remove_file(wal_path);
    }

    #[test]
    fn wal_rejects_corrupt_complete_length_header() {
        let wal_path = test_path("corrupt-length");
        let mut wal = CacheWal::create_after(&wal_path, 0).unwrap();
        wal.append(
            &CacheWalMutation::SetInteger {
                key: b"k".to_vec(),
                value: 1,
                expires_unix_ms: None,
            },
            CacheWalSync::Data,
        )
        .unwrap();
        drop(wal);

        let mut bytes = fs::read(&wal_path).unwrap();
        bytes[WAL_HEADER_BYTES + 4] ^= 0x01;
        fs::write(&wal_path, bytes).unwrap();

        let error = CacheWal::open(&wal_path).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);

        let _ = fs::remove_file(wal_path);
    }

    #[test]
    fn expired_wal_set_does_not_resurrect_older_snapshot_value() {
        let snapshot = test_path("expired-snapshot");
        let wal_path = test_path("expired-wal");
        let mut store = CacheStore::new();
        store.set_bytes(b"k", b"old", None, 0);
        write_cache_snapshot(&snapshot, &store, 0, 0, 1_000).unwrap();

        let mut wal = CacheWal::create_after(&wal_path, 0).unwrap();
        wal.append(
            &CacheWalMutation::SetBytes {
                key: b"k".to_vec(),
                value: b"new".to_vec(),
                expires_unix_ms: Some(1_500),
            },
            CacheWalSync::Data,
        )
        .unwrap();
        drop(wal);

        let (mut recovered, _) = recover_cache(
            &snapshot,
            &wal_path,
            CacheConfig::default(),
            CacheEvictionPolicy::S3Fifo,
            0,
            2_000,
        )
        .unwrap();
        assert_eq!(recovered.get(b"k", 0), None);

        let _ = fs::remove_file(snapshot);
        let _ = fs::remove_file(wal_path);
    }

    #[test]
    fn wal_rotation_base_sequence_requires_matching_snapshot() {
        let wal_path = test_path("rotated-wal");
        let snapshot = test_path("rotated-snapshot");
        let wal = CacheWal::create_after(&wal_path, 5).unwrap();
        assert_eq!(wal.base_sequence(), 5);
        drop(wal);

        let error = recover_cache(
            &snapshot,
            &wal_path,
            CacheConfig::default(),
            CacheEvictionPolicy::S3Fifo,
            0,
            0,
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);

        let _ = fs::remove_file(wal_path);
    }
}
