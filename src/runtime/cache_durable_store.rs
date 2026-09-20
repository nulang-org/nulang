//! Cold-path durable snapshot container for the RESP cache tier.
//!
//! This module does not put filesystem I/O on GET/SET. A caller explicitly
//! snapshots a shard-local CacheStore, then may restore it into a fresh reactor.
//! Exact CacheTransferToken slot/generation identities survive the round trip so
//! in-flight migration ACKs remain generation-fenced after process restart.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use super::cache::{
    CacheDurableEntry, CacheDurableRestoreError, CacheStore, CacheTransferToken, CacheTransferValue,
};

const MAGIC: &[u8; 4] = b"NCDS";
const VERSION: u8 = 1;
const CHECKSUM_LEN: usize = 32;
const MAX_SNAPSHOT_BYTES: usize = 1024 * 1024 * 1024;
const MAX_ENTRIES: usize = 16_777_216;
const MAX_KEY_BYTES: usize = 64 * 1024 * 1024;
const MAX_VALUE_BYTES: usize = 512 * 1024 * 1024;

#[derive(Debug)]
pub enum CacheSnapshotError {
    Io(io::Error),
    InvalidHeader,
    Truncated,
    TrailingBytes,
    ChecksumMismatch,
    TooLarge,
    TooManyEntries,
    InvalidValueKind(u8),
    InvalidExpiryTag(u8),
    LengthOverflow,
    Restore(CacheDurableRestoreError),
}

impl From<io::Error> for CacheSnapshotError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<CacheDurableRestoreError> for CacheSnapshotError {
    fn from(value: CacheDurableRestoreError) -> Self {
        Self::Restore(value)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheSnapshotReport {
    pub entries: usize,
    pub bytes: u64,
    pub captured_unix_ms: u64,
}

pub fn write_cache_snapshot(
    path: impl AsRef<Path>,
    store: &CacheStore,
    now_ms: u64,
) -> Result<CacheSnapshotReport, CacheSnapshotError> {
    let captured_unix_ms = current_unix_ms()?;
    write_cache_snapshot_at(path, store, now_ms, captured_unix_ms)
}

/// Write a snapshot with an explicit wall-clock anchor captured no later than
/// the supplied monotonic `now_ms` observation.
///
/// Reactor callers should capture wall time first, then monotonic time, so
/// downtime reconstruction can only shorten TTL rather than extend it.
pub fn write_cache_snapshot_at(
    path: impl AsRef<Path>,
    store: &CacheStore,
    now_ms: u64,
    captured_unix_ms: u64,
) -> Result<CacheSnapshotReport, CacheSnapshotError> {
    let path = path.as_ref();
    let entries = store.export_durable_entries(now_ms, captured_unix_ms);
    let bytes = encode_snapshot(&entries)?;

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temp = snapshot_temp_path(path);
    let write_result = (|| -> Result<(), CacheSnapshotError> {
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&temp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(&temp, path)?;
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                File::open(parent)?.sync_all()?;
            }
        }
        Ok(())
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    write_result?;

    Ok(CacheSnapshotReport {
        entries: entries.len(),
        bytes: bytes.len() as u64,
        captured_unix_ms,
    })
}

pub fn restore_cache_snapshot(
    path: impl AsRef<Path>,
    now_ms: u64,
) -> Result<CacheStore, CacheSnapshotError> {
    let mut file = File::open(path)?;
    let metadata = file.metadata()?;
    if metadata.len() > MAX_SNAPSHOT_BYTES as u64 {
        return Err(CacheSnapshotError::TooLarge);
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.read_to_end(&mut bytes)?;
    let entries = decode_snapshot(&bytes)?;
    let wall_now = current_unix_ms()?;
    Ok(CacheStore::restore_durable_entries(
        &entries, now_ms, wall_now,
    )?)
}

pub fn encode_snapshot(entries: &[CacheDurableEntry]) -> Result<Vec<u8>, CacheSnapshotError> {
    if entries.len() > MAX_ENTRIES {
        return Err(CacheSnapshotError::TooManyEntries);
    }
    let mut body = Vec::new();
    write_u32(
        &mut body,
        u32::try_from(entries.len()).map_err(|_| CacheSnapshotError::TooManyEntries)?,
    );
    for entry in entries {
        write_blob(&mut body, &entry.key, MAX_KEY_BYTES)?;
        match &entry.value {
            CacheTransferValue::Integer(value) => {
                body.push(0);
                body.extend_from_slice(&value.to_be_bytes());
            }
            CacheTransferValue::Bytes(value) => {
                body.push(1);
                write_blob(&mut body, value, MAX_VALUE_BYTES)?;
            }
        }
        match entry.expires_unix_ms {
            None => body.push(0),
            Some(deadline) => {
                body.push(1);
                write_u64(&mut body, deadline);
            }
        }
        write_u32(&mut body, entry.token.source_slot);
        write_u32(&mut body, entry.token.source_generation);
        if body.len() > MAX_SNAPSHOT_BYTES {
            return Err(CacheSnapshotError::TooLarge);
        }
    }

    let mut out = Vec::with_capacity(5 + body.len() + CHECKSUM_LEN);
    out.extend_from_slice(MAGIC);
    out.push(VERSION);
    out.extend_from_slice(&body);
    let checksum = blake3::hash(&out);
    out.extend_from_slice(checksum.as_bytes());
    if out.len() > MAX_SNAPSHOT_BYTES {
        return Err(CacheSnapshotError::TooLarge);
    }
    Ok(out)
}

pub fn decode_snapshot(bytes: &[u8]) -> Result<Vec<CacheDurableEntry>, CacheSnapshotError> {
    if bytes.len() > MAX_SNAPSHOT_BYTES {
        return Err(CacheSnapshotError::TooLarge);
    }
    if bytes.len() < 5 + CHECKSUM_LEN {
        return Err(CacheSnapshotError::Truncated);
    }
    if &bytes[..4] != MAGIC || bytes[4] != VERSION {
        return Err(CacheSnapshotError::InvalidHeader);
    }
    let data_end = bytes.len() - CHECKSUM_LEN;
    let expected = blake3::hash(&bytes[..data_end]);
    if bytes[data_end..] != expected.as_bytes()[..] {
        return Err(CacheSnapshotError::ChecksumMismatch);
    }

    let mut reader = Reader::new(&bytes[5..data_end]);
    let count = reader.u32()? as usize;
    if count > MAX_ENTRIES {
        return Err(CacheSnapshotError::TooManyEntries);
    }
    let mut entries = Vec::with_capacity(count);
    for _ in 0..count {
        let key = reader.blob(MAX_KEY_BYTES)?;
        let value = match reader.u8()? {
            0 => CacheTransferValue::Integer(reader.i64()?),
            1 => CacheTransferValue::Bytes(reader.blob(MAX_VALUE_BYTES)?),
            other => return Err(CacheSnapshotError::InvalidValueKind(other)),
        };
        let expires_unix_ms = match reader.u8()? {
            0 => None,
            1 => Some(reader.u64()?),
            other => return Err(CacheSnapshotError::InvalidExpiryTag(other)),
        };
        let source_slot = reader.u32()?;
        let source_generation = reader.u32()?;
        entries.push(CacheDurableEntry {
            key,
            value,
            expires_unix_ms,
            token: CacheTransferToken {
                source_slot,
                source_generation,
            },
        });
    }
    reader.finish()?;
    Ok(entries)
}

fn current_unix_ms() -> Result<u64, CacheSnapshotError> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| CacheSnapshotError::InvalidHeader)?;
    u64::try_from(duration.as_millis()).map_err(|_| CacheSnapshotError::LengthOverflow)
}

fn snapshot_temp_path(path: &Path) -> PathBuf {
    let mut temp = path.as_os_str().to_os_string();
    temp.push(".tmp");
    PathBuf::from(temp)
}

fn write_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_be_bytes());
}

fn write_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_be_bytes());
}

fn write_blob(out: &mut Vec<u8>, bytes: &[u8], max: usize) -> Result<(), CacheSnapshotError> {
    if bytes.len() > max {
        return Err(CacheSnapshotError::TooLarge);
    }
    let len = u32::try_from(bytes.len()).map_err(|_| CacheSnapshotError::LengthOverflow)?;
    write_u32(out, len);
    out.extend_from_slice(bytes);
    Ok(())
}

struct Reader<'a> {
    bytes: &'a [u8],
    cursor: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, cursor: 0 }
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], CacheSnapshotError> {
        let end = self
            .cursor
            .checked_add(len)
            .ok_or(CacheSnapshotError::LengthOverflow)?;
        if end > self.bytes.len() {
            return Err(CacheSnapshotError::Truncated);
        }
        let slice = &self.bytes[self.cursor..end];
        self.cursor = end;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8, CacheSnapshotError> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32, CacheSnapshotError> {
        Ok(u32::from_be_bytes(
            self.take(4)?.try_into().expect("u32 slice"),
        ))
    }

    fn u64(&mut self) -> Result<u64, CacheSnapshotError> {
        Ok(u64::from_be_bytes(
            self.take(8)?.try_into().expect("u64 slice"),
        ))
    }

    fn i64(&mut self) -> Result<i64, CacheSnapshotError> {
        Ok(i64::from_be_bytes(
            self.take(8)?.try_into().expect("i64 slice"),
        ))
    }

    fn blob(&mut self, max: usize) -> Result<Vec<u8>, CacheSnapshotError> {
        let len = self.u32()? as usize;
        if len > max {
            return Err(CacheSnapshotError::TooLarge);
        }
        Ok(self.take(len)?.to_vec())
    }

    fn finish(self) -> Result<(), CacheSnapshotError> {
        if self.cursor == self.bytes.len() {
            Ok(())
        } else {
            Err(CacheSnapshotError::TrailingBytes)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::cache::{CacheTransferFinalize, CacheTtl, CacheValueView};
    use super::*;
    use std::time::{Duration, SystemTime};

    fn temp_path(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "nulang-cache-{name}-{}-{nanos}.snapshot",
            std::process::id()
        ))
    }

    #[test]
    fn codec_round_trip_preserves_entries_and_tokens() {
        let entries = vec![
            CacheDurableEntry {
                key: b"a".to_vec(),
                value: CacheTransferValue::Integer(7),
                expires_unix_ms: None,
                token: CacheTransferToken {
                    source_slot: 3,
                    source_generation: 9,
                },
            },
            CacheDurableEntry {
                key: b"b".to_vec(),
                value: CacheTransferValue::Bytes(b"value".to_vec()),
                expires_unix_ms: Some(123_456),
                token: CacheTransferToken {
                    source_slot: 8,
                    source_generation: 11,
                },
            },
        ];
        let encoded = encode_snapshot(&entries).unwrap();
        assert_eq!(decode_snapshot(&encoded).unwrap(), entries);
    }

    #[test]
    fn checksum_corruption_fails_closed() {
        let entries = vec![CacheDurableEntry {
            key: b"k".to_vec(),
            value: CacheTransferValue::Integer(1),
            expires_unix_ms: None,
            token: CacheTransferToken {
                source_slot: 0,
                source_generation: 1,
            },
        }];
        let mut encoded = encode_snapshot(&entries).unwrap();
        encoded[8] ^= 0x80;
        assert!(matches!(
            decode_snapshot(&encoded),
            Err(CacheSnapshotError::ChecksumMismatch)
        ));
    }

    #[test]
    fn file_snapshot_restores_values_ttl_and_migration_tokens() {
        let path = temp_path("roundtrip");
        let mut source = CacheStore::new();
        source.set_bytes(b"persist", b"value", None, 0);
        source.set_integer(b"ttl", 42, Some(10_000), 0);

        let transfer = source
            .export_slot_batch(super::super::cache::redis_slot(b"persist"), None, 1, 0)
            .entries
            .into_iter()
            .find(|entry| entry.key == b"persist")
            .unwrap();
        let report = write_cache_snapshot(&path, &source, 0).unwrap();
        assert_eq!(report.entries, 2);
        assert!(report.bytes > 0);

        std::thread::sleep(Duration::from_millis(5));
        let mut restored = restore_cache_snapshot(&path, 100).unwrap();
        assert_eq!(
            restored.get(b"persist", 100),
            Some(CacheValueView::Bytes(b"value"))
        );
        assert_eq!(restored.get(b"ttl", 100), Some(CacheValueView::Integer(42)));
        assert!(matches!(
            restored.ttl(b"ttl", 100),
            CacheTtl::RemainingMs(ttl) if ttl < 10_000 && ttl > 9_000
        ));
        assert_eq!(
            restored.finalize_transfer_entry(&transfer, 100),
            CacheTransferFinalize::Removed
        );

        fs::remove_file(path).unwrap();
    }
}
