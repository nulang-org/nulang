//! Append-only mutation WAL for the RESP cache tier.
//!
//! WAL records describe exact post-mutation physical state rather than RESP
//! commands. This preserves CacheTransferToken slot/generation identities
//! across replay and keeps migration fencing correct after restart.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use super::cache::{
    CacheDurableEntry, CacheDurableRestoreError, CacheStore, CacheTransferToken, CacheTransferValue,
};

const MAGIC: &[u8; 4] = b"NCWL";
const VERSION: u8 = 1;
const HEADER_LEN: usize = 5;
const RECORD_PREFIX_LEN: usize = 13; // kind + len + lsn
const CHECKSUM_LEN: usize = 32;
const KIND_UPSERT: u8 = 1;
const KIND_DELETE: u8 = 2;
const KIND_BATCH: u8 = 3;
const MAX_RECORD_BYTES: usize = 512 * 1024 * 1024;
const MAX_KEY_BYTES: usize = 64 * 1024 * 1024;
const MAX_VALUE_BYTES: usize = 384 * 1024 * 1024;

#[derive(Debug)]
pub enum CacheWalError {
    Io(io::Error),
    InvalidHeader,
    Truncated,
    ChecksumMismatch,
    UnknownKind(u8),
    InvalidValueKind(u8),
    InvalidExpiryTag(u8),
    TooLarge,
    LengthOverflow,
    NonMonotonicLsn { previous: u64, next: u64 },
    LsnExhausted,
    Restore(CacheDurableRestoreError),
}

impl From<io::Error> for CacheWalError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<CacheDurableRestoreError> for CacheWalError {
    fn from(value: CacheDurableRestoreError) -> Self {
        Self::Restore(value)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheWalSync {
    Buffered,
    Fsync,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheWalMutation {
    Upsert(CacheDurableEntry),
    Delete {
        key: Vec<u8>,
        token: CacheTransferToken,
    },
    Batch(Vec<CacheWalMutation>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheWalRecord {
    pub lsn: u64,
    pub mutation: CacheWalMutation,
}

#[derive(Debug)]
pub struct CacheWal {
    path: PathBuf,
    file: File,
    last_lsn: u64,
}

impl CacheWal {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, CacheWalError> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&path)?;

        if file.metadata()?.len() == 0 {
            file.write_all(MAGIC)?;
            file.write_all(&[VERSION])?;
            file.sync_all()?;
        }

        file.seek(SeekFrom::Start(0))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        if bytes.len() < HEADER_LEN || &bytes[..4] != MAGIC || bytes[4] != VERSION {
            return Err(CacheWalError::InvalidHeader);
        }

        let mut cursor = HEADER_LEN;
        let mut valid_end = cursor;
        let mut last_lsn = 0u64;
        while cursor < bytes.len() {
            if bytes.len() - cursor < RECORD_PREFIX_LEN {
                break;
            }
            let kind = bytes[cursor];
            let len = u32::from_be_bytes(
                bytes[cursor + 1..cursor + 5]
                    .try_into()
                    .expect("WAL len slice"),
            ) as usize;
            let lsn = u64::from_be_bytes(
                bytes[cursor + 5..cursor + 13]
                    .try_into()
                    .expect("WAL lsn slice"),
            );
            if len > MAX_RECORD_BYTES {
                return Err(CacheWalError::TooLarge);
            }
            if lsn <= last_lsn {
                return Err(CacheWalError::NonMonotonicLsn {
                    previous: last_lsn,
                    next: lsn,
                });
            }
            let payload_start = cursor + RECORD_PREFIX_LEN;
            let payload_end = payload_start
                .checked_add(len)
                .ok_or(CacheWalError::LengthOverflow)?;
            let record_end = payload_end
                .checked_add(CHECKSUM_LEN)
                .ok_or(CacheWalError::LengthOverflow)?;
            if record_end > bytes.len() {
                break;
            }

            let expected =
                record_checksum(kind, len as u32, lsn, &bytes[payload_start..payload_end]);
            if bytes[payload_end..record_end] != expected {
                return Err(CacheWalError::ChecksumMismatch);
            }
            // Decode complete records during open so malformed-but-checksummed
            // payloads fail before the WAL is accepted.
            let _ = decode_mutation(kind, &bytes[payload_start..payload_end])?;

            last_lsn = lsn;
            cursor = record_end;
            valid_end = cursor;
        }

        if valid_end < bytes.len() {
            file.set_len(valid_end as u64)?;
            file.sync_all()?;
        }
        file.seek(SeekFrom::End(0))?;

        Ok(Self {
            path,
            file,
            last_lsn,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn last_lsn(&self) -> u64 {
        self.last_lsn
    }

    pub fn append_upsert(
        &mut self,
        entry: &CacheDurableEntry,
        sync: CacheWalSync,
    ) -> Result<u64, CacheWalError> {
        self.append(CacheWalMutation::Upsert(entry.clone()), sync)
    }

    pub fn append_delete(
        &mut self,
        key: &[u8],
        token: CacheTransferToken,
        sync: CacheWalSync,
    ) -> Result<u64, CacheWalError> {
        self.append(
            CacheWalMutation::Delete {
                key: key.to_vec(),
                token,
            },
            sync,
        )
    }

    pub fn append_batch(
        &mut self,
        mutations: Vec<CacheWalMutation>,
        sync: CacheWalSync,
    ) -> Result<u64, CacheWalError> {
        self.append(CacheWalMutation::Batch(mutations), sync)
    }

    pub fn append(
        &mut self,
        mutation: CacheWalMutation,
        sync: CacheWalSync,
    ) -> Result<u64, CacheWalError> {
        let lsn = self
            .last_lsn
            .checked_add(1)
            .ok_or(CacheWalError::LsnExhausted)?;
        let (kind, payload) = encode_mutation(&mutation)?;
        let len = u32::try_from(payload.len()).map_err(|_| CacheWalError::LengthOverflow)?;
        let checksum = record_checksum(kind, len, lsn, &payload);

        self.file.write_all(&[kind])?;
        self.file.write_all(&len.to_be_bytes())?;
        self.file.write_all(&lsn.to_be_bytes())?;
        self.file.write_all(&payload)?;
        self.file.write_all(&checksum)?;
        if sync == CacheWalSync::Fsync {
            self.file.sync_data()?;
        }
        self.last_lsn = lsn;
        Ok(lsn)
    }

    pub fn sync(&mut self) -> Result<(), CacheWalError> {
        self.file.sync_data()?;
        Ok(())
    }

    pub fn read_records_after(
        &mut self,
        checkpoint_lsn: u64,
    ) -> Result<Vec<CacheWalRecord>, CacheWalError> {
        self.file.flush()?;
        self.file.seek(SeekFrom::Start(0))?;
        let mut bytes = Vec::new();
        self.file.read_to_end(&mut bytes)?;
        let records = decode_records(&bytes)?
            .into_iter()
            .filter(|record| record.lsn > checkpoint_lsn)
            .collect();
        self.file.seek(SeekFrom::End(0))?;
        Ok(records)
    }

    pub fn replay_into(
        &mut self,
        store: &mut CacheStore,
        checkpoint_lsn: u64,
        now_ms: u64,
        wall_now_unix_ms: u64,
    ) -> Result<u64, CacheWalError> {
        let records = self.read_records_after(checkpoint_lsn)?;
        let mut applied = checkpoint_lsn;
        for record in records {
            apply_mutation(store, record.mutation, now_ms, wall_now_unix_ms)?;
            applied = record.lsn;
        }
        Ok(applied)
    }
}

pub fn current_unix_ms() -> Result<u64, CacheWalError> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| CacheWalError::InvalidHeader)?;
    u64::try_from(duration.as_millis()).map_err(|_| CacheWalError::LengthOverflow)
}

fn decode_records(bytes: &[u8]) -> Result<Vec<CacheWalRecord>, CacheWalError> {
    if bytes.len() < HEADER_LEN || &bytes[..4] != MAGIC || bytes[4] != VERSION {
        return Err(CacheWalError::InvalidHeader);
    }
    let mut records = Vec::new();
    let mut cursor = HEADER_LEN;
    let mut previous = 0u64;
    while cursor < bytes.len() {
        if bytes.len() - cursor < RECORD_PREFIX_LEN {
            return Err(CacheWalError::Truncated);
        }
        let kind = bytes[cursor];
        let len = u32::from_be_bytes(
            bytes[cursor + 1..cursor + 5]
                .try_into()
                .expect("WAL len slice"),
        ) as usize;
        let lsn = u64::from_be_bytes(
            bytes[cursor + 5..cursor + 13]
                .try_into()
                .expect("WAL lsn slice"),
        );
        if len > MAX_RECORD_BYTES {
            return Err(CacheWalError::TooLarge);
        }
        if lsn <= previous {
            return Err(CacheWalError::NonMonotonicLsn {
                previous,
                next: lsn,
            });
        }
        let payload_start = cursor + RECORD_PREFIX_LEN;
        let payload_end = payload_start
            .checked_add(len)
            .ok_or(CacheWalError::LengthOverflow)?;
        let record_end = payload_end
            .checked_add(CHECKSUM_LEN)
            .ok_or(CacheWalError::LengthOverflow)?;
        if record_end > bytes.len() {
            return Err(CacheWalError::Truncated);
        }
        let expected = record_checksum(kind, len as u32, lsn, &bytes[payload_start..payload_end]);
        if bytes[payload_end..record_end] != expected {
            return Err(CacheWalError::ChecksumMismatch);
        }
        records.push(CacheWalRecord {
            lsn,
            mutation: decode_mutation(kind, &bytes[payload_start..payload_end])?,
        });
        previous = lsn;
        cursor = record_end;
    }
    Ok(records)
}

fn encode_mutation(mutation: &CacheWalMutation) -> Result<(u8, Vec<u8>), CacheWalError> {
    let mut out = Vec::new();
    match mutation {
        CacheWalMutation::Upsert(entry) => {
            write_blob(&mut out, &entry.key, MAX_KEY_BYTES)?;
            match &entry.value {
                CacheTransferValue::Integer(value) => {
                    out.push(0);
                    out.extend_from_slice(&value.to_be_bytes());
                }
                CacheTransferValue::Bytes(value) => {
                    out.push(1);
                    write_blob(&mut out, value, MAX_VALUE_BYTES)?;
                }
            }
            match entry.expires_unix_ms {
                None => out.push(0),
                Some(deadline) => {
                    out.push(1);
                    out.extend_from_slice(&deadline.to_be_bytes());
                }
            }
            out.extend_from_slice(&entry.token.source_slot.to_be_bytes());
            out.extend_from_slice(&entry.token.source_generation.to_be_bytes());
            Ok((KIND_UPSERT, out))
        }
        CacheWalMutation::Delete { key, token } => {
            write_blob(&mut out, key, MAX_KEY_BYTES)?;
            out.extend_from_slice(&token.source_slot.to_be_bytes());
            out.extend_from_slice(&token.source_generation.to_be_bytes());
            Ok((KIND_DELETE, out))
        }
        CacheWalMutation::Batch(mutations) => {
            let count =
                u32::try_from(mutations.len()).map_err(|_| CacheWalError::LengthOverflow)?;
            out.extend_from_slice(&count.to_be_bytes());
            for mutation in mutations {
                let (subkind, payload) = encode_mutation(mutation)?;
                if subkind == KIND_BATCH {
                    return Err(CacheWalError::TooLarge);
                }
                out.push(subkind);
                let len =
                    u32::try_from(payload.len()).map_err(|_| CacheWalError::LengthOverflow)?;
                out.extend_from_slice(&len.to_be_bytes());
                out.extend_from_slice(&payload);
                if out.len() > MAX_RECORD_BYTES {
                    return Err(CacheWalError::TooLarge);
                }
            }
            Ok((KIND_BATCH, out))
        }
    }
}

fn decode_mutation(kind: u8, payload: &[u8]) -> Result<CacheWalMutation, CacheWalError> {
    let mut reader = Reader::new(payload);
    let mutation = match kind {
        KIND_UPSERT => {
            let key = reader.blob(MAX_KEY_BYTES)?;
            let value = match reader.u8()? {
                0 => CacheTransferValue::Integer(reader.i64()?),
                1 => CacheTransferValue::Bytes(reader.blob(MAX_VALUE_BYTES)?),
                other => return Err(CacheWalError::InvalidValueKind(other)),
            };
            let expires_unix_ms = match reader.u8()? {
                0 => None,
                1 => Some(reader.u64()?),
                other => return Err(CacheWalError::InvalidExpiryTag(other)),
            };
            CacheWalMutation::Upsert(CacheDurableEntry {
                key,
                value,
                expires_unix_ms,
                token: CacheTransferToken {
                    source_slot: reader.u32()?,
                    source_generation: reader.u32()?,
                },
            })
        }
        KIND_DELETE => CacheWalMutation::Delete {
            key: reader.blob(MAX_KEY_BYTES)?,
            token: CacheTransferToken {
                source_slot: reader.u32()?,
                source_generation: reader.u32()?,
            },
        },
        KIND_BATCH => {
            let count = reader.u32()? as usize;
            let mut mutations = Vec::with_capacity(count);
            for _ in 0..count {
                let subkind = reader.u8()?;
                if subkind == KIND_BATCH {
                    return Err(CacheWalError::UnknownKind(subkind));
                }
                let len = reader.u32()? as usize;
                if len > MAX_RECORD_BYTES {
                    return Err(CacheWalError::TooLarge);
                }
                let payload = reader.take(len)?;
                mutations.push(decode_mutation(subkind, payload)?);
            }
            CacheWalMutation::Batch(mutations)
        }
        other => return Err(CacheWalError::UnknownKind(other)),
    };
    reader.finish()?;
    Ok(mutation)
}

fn apply_mutation(
    store: &mut CacheStore,
    mutation: CacheWalMutation,
    now_ms: u64,
    wall_now_unix_ms: u64,
) -> Result<(), CacheWalError> {
    match mutation {
        CacheWalMutation::Upsert(entry) => {
            store.apply_durable_entry(&entry, now_ms, wall_now_unix_ms)?;
        }
        CacheWalMutation::Delete { key, token } => {
            let _ = store.delete_durable_token(&key, token);
        }
        CacheWalMutation::Batch(mutations) => {
            for mutation in mutations {
                apply_mutation(store, mutation, now_ms, wall_now_unix_ms)?;
            }
        }
    }
    Ok(())
}

fn record_checksum(kind: u8, len: u32, lsn: u64, payload: &[u8]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&[kind]);
    hasher.update(&len.to_be_bytes());
    hasher.update(&lsn.to_be_bytes());
    hasher.update(payload);
    *hasher.finalize().as_bytes()
}

fn write_blob(out: &mut Vec<u8>, bytes: &[u8], max: usize) -> Result<(), CacheWalError> {
    if bytes.len() > max {
        return Err(CacheWalError::TooLarge);
    }
    let len = u32::try_from(bytes.len()).map_err(|_| CacheWalError::LengthOverflow)?;
    out.extend_from_slice(&len.to_be_bytes());
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

    fn take(&mut self, len: usize) -> Result<&'a [u8], CacheWalError> {
        let end = self
            .cursor
            .checked_add(len)
            .ok_or(CacheWalError::LengthOverflow)?;
        if end > self.bytes.len() {
            return Err(CacheWalError::Truncated);
        }
        let slice = &self.bytes[self.cursor..end];
        self.cursor = end;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8, CacheWalError> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32, CacheWalError> {
        Ok(u32::from_be_bytes(
            self.take(4)?.try_into().expect("u32 slice"),
        ))
    }

    fn u64(&mut self) -> Result<u64, CacheWalError> {
        Ok(u64::from_be_bytes(
            self.take(8)?.try_into().expect("u64 slice"),
        ))
    }

    fn i64(&mut self) -> Result<i64, CacheWalError> {
        Ok(i64::from_be_bytes(
            self.take(8)?.try_into().expect("i64 slice"),
        ))
    }

    fn blob(&mut self, max: usize) -> Result<Vec<u8>, CacheWalError> {
        let len = self.u32()? as usize;
        if len > max {
            return Err(CacheWalError::TooLarge);
        }
        Ok(self.take(len)?.to_vec())
    }

    fn finish(self) -> Result<(), CacheWalError> {
        if self.cursor == self.bytes.len() {
            Ok(())
        } else {
            Err(CacheWalError::TooLarge)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::cache::{CacheTtl, CacheValueView};
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_path(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "nulang-cache-{name}-{}-{nonce}.wal",
            std::process::id()
        ))
    }

    fn upsert(key: &[u8], value: &[u8], slot: u32, generation: u32) -> CacheDurableEntry {
        CacheDurableEntry {
            key: key.to_vec(),
            value: CacheTransferValue::Bytes(value.to_vec()),
            expires_unix_ms: None,
            token: CacheTransferToken {
                source_slot: slot,
                source_generation: generation,
            },
        }
    }

    #[test]
    fn wal_replays_exact_post_mutation_tokens_after_checkpoint_boundary() {
        let path = temp_path("replay");
        {
            let mut wal = CacheWal::open(&path).unwrap();
            assert_eq!(
                wal.append_upsert(&upsert(b"a", b"old", 0, 1), CacheWalSync::Fsync)
                    .unwrap(),
                1
            );
            assert_eq!(
                wal.append_upsert(&upsert(b"a", b"new", 0, 2), CacheWalSync::Fsync)
                    .unwrap(),
                2
            );
            assert_eq!(
                wal.append_upsert(&upsert(b"b", b"value", 1, 1), CacheWalSync::Fsync)
                    .unwrap(),
                3
            );
            assert_eq!(
                wal.append_delete(
                    b"a",
                    CacheTransferToken {
                        source_slot: 0,
                        source_generation: 2,
                    },
                    CacheWalSync::Fsync,
                )
                .unwrap(),
                4
            );
        }

        let mut checkpoint = CacheStore::new();
        checkpoint
            .apply_durable_entry(&upsert(b"a", b"old", 0, 1), 0, 10_000)
            .unwrap();
        let mut wal = CacheWal::open(&path).unwrap();
        assert_eq!(wal.replay_into(&mut checkpoint, 1, 0, 10_000).unwrap(), 4);
        assert!(!checkpoint.exists(b"a", 0));
        assert_eq!(
            checkpoint.get(b"b", 0),
            Some(CacheValueView::Bytes(b"value"))
        );
        assert_eq!(
            checkpoint
                .durable_entry_for_key(b"b", 0, 10_000)
                .unwrap()
                .token,
            CacheTransferToken {
                source_slot: 1,
                source_generation: 1,
            }
        );
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn snapshot_lsn_boundary_prevents_double_applying_wal_prefix() {
        let snapshot_path = temp_path("checkpoint-boundary").with_extension("snapshot");
        let wal_path = temp_path("checkpoint-boundary");
        let wall = current_unix_ms().unwrap();

        let mut store = CacheStore::new();
        let mut wal = CacheWal::open(&wal_path).unwrap();

        store.set_bytes(b"k", b"one", None, 0);
        let first = store.durable_entry_for_key(b"k", 0, wall).unwrap();
        assert_eq!(wal.append_upsert(&first, CacheWalSync::Fsync).unwrap(), 1);

        super::super::cache_durable_store::write_cache_snapshot_at_lsn(
            &snapshot_path,
            &store,
            0,
            wal.last_lsn(),
        )
        .unwrap();

        store.set_bytes(b"k", b"two", None, 1);
        let second = store.durable_entry_for_key(b"k", 1, wall + 1).unwrap();
        assert_eq!(wal.append_upsert(&second, CacheWalSync::Fsync).unwrap(), 2);

        let restored =
            super::super::cache_durable_store::restore_cache_snapshot_with_lsn(&snapshot_path, 0)
                .unwrap();
        assert_eq!(restored.checkpoint_lsn, 1);
        let mut recovered = restored.store;
        let applied = wal
            .replay_into(&mut recovered, restored.checkpoint_lsn, 0, wall + 2)
            .unwrap();
        assert_eq!(applied, 2);
        assert_eq!(recovered.get(b"k", 0), Some(CacheValueView::Bytes(b"two")));
        assert_eq!(
            recovered
                .durable_entry_for_key(b"k", 0, wall + 2)
                .unwrap()
                .token,
            second.token
        );

        fs::remove_file(snapshot_path).unwrap();
        fs::remove_file(wal_path).unwrap();
    }

    #[test]
    fn batch_record_replays_multi_key_mutation_under_one_lsn() {
        let path = temp_path("batch");
        let mut wal = CacheWal::open(&path).unwrap();
        let lsn = wal
            .append_batch(
                vec![
                    CacheWalMutation::Upsert(upsert(b"a", b"1", 0, 1)),
                    CacheWalMutation::Upsert(upsert(b"b", b"2", 1, 1)),
                ],
                CacheWalSync::Fsync,
            )
            .unwrap();
        assert_eq!(lsn, 1);

        let mut store = CacheStore::new();
        assert_eq!(wal.replay_into(&mut store, 0, 0, 10_000).unwrap(), 1);
        assert_eq!(store.get(b"a", 0), Some(CacheValueView::Bytes(b"1")));
        assert_eq!(store.get(b"b", 0), Some(CacheValueView::Bytes(b"2")));
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn wal_recovery_truncates_crash_tail_but_rejects_complete_corruption() {
        let path = temp_path("tail");
        {
            let mut wal = CacheWal::open(&path).unwrap();
            wal.append_upsert(&upsert(b"k", b"v", 0, 1), CacheWalSync::Fsync)
                .unwrap();
        }
        {
            let mut file = OpenOptions::new().append(true).open(&path).unwrap();
            file.write_all(&[KIND_DELETE, 0, 0]).unwrap();
            file.sync_all().unwrap();
        }
        let wal = CacheWal::open(&path).unwrap();
        assert_eq!(wal.last_lsn(), 1);
        drop(wal);

        let mut bytes = fs::read(&path).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0x80;
        fs::write(&path, bytes).unwrap();
        assert!(matches!(
            CacheWal::open(&path),
            Err(CacheWalError::ChecksumMismatch)
        ));
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn wal_replay_reconstructs_absolute_ttl_without_resetting_lifetime() {
        let path = temp_path("ttl");
        let entry = CacheDurableEntry {
            key: b"ttl".to_vec(),
            value: CacheTransferValue::Integer(9),
            expires_unix_ms: Some(15_000),
            token: CacheTransferToken {
                source_slot: 0,
                source_generation: 1,
            },
        };
        let mut wal = CacheWal::open(&path).unwrap();
        wal.append_upsert(&entry, CacheWalSync::Fsync).unwrap();

        let mut store = CacheStore::new();
        wal.replay_into(&mut store, 0, 100, 12_000).unwrap();
        assert_eq!(store.get(b"ttl", 100), Some(CacheValueView::Integer(9)));
        assert_eq!(store.ttl(b"ttl", 100), CacheTtl::RemainingMs(3_000));

        let mut expired = CacheStore::new();
        wal.replay_into(&mut expired, 0, 0, 16_000).unwrap();
        assert!(!expired.exists(b"ttl", 0));
        fs::remove_file(path).unwrap();
    }
}
