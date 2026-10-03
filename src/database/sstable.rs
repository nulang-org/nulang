//! Immutable NuDB SSTable v1 format and block-backed reader.
//!
//! The on-disk format remains `NUDBSST1`. Open validates the complete payload
//! checksum while building a compact logical block index. Row/value bodies are
//! not retained after open; reads load one indexed block, verify its block
//! checksum, and optionally retain the raw block in a bounded shared cache.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use super::tablet::{TabletSnapshotRow, VersionedValue};

const SSTABLE_MAGIC: &[u8; 8] = b"NUDBSST1";
const SSTABLE_VERSION: u16 = 1;
const MAX_SSTABLE_BYTES: usize = 256 * 1024 * 1024;
const MAX_ROWS: usize = 4_000_000;
const MAX_VERSIONS_PER_ROW: usize = 65_536;
const MAX_KEY_BYTES: usize = 16 * 1024 * 1024;
const MAX_VALUE_BYTES: usize = 64 * 1024 * 1024;
const TARGET_BLOCK_BYTES: usize = 64 * 1024;
pub(crate) const DEFAULT_BLOCK_CACHE_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SstableMetadata {
    pub(crate) tablet_id: u64,
    pub(crate) ownership_epoch: u64,
    pub(crate) min_sequence: u64,
    pub(crate) max_sequence: u64,
    pub(crate) row_count: u32,
    pub(crate) min_key: Vec<u8>,
    pub(crate) max_key: Vec<u8>,
    pub(crate) checksum: [u8; 32],
    pub(crate) file_name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct SstableCacheStats {
    pub(crate) max_bytes: usize,
    pub(crate) resident_bytes: usize,
    pub(crate) entries: usize,
    pub(crate) hits: u64,
    pub(crate) misses: u64,
}

type CacheKey = ([u8; 32], u32);

#[derive(Debug)]
pub(crate) struct SstableBlockCache {
    max_bytes: usize,
    resident_bytes: usize,
    entries: BTreeMap<CacheKey, Arc<[u8]>>,
    lru: VecDeque<CacheKey>,
    hits: u64,
    misses: u64,
}

impl SstableBlockCache {
    pub(crate) fn new(max_bytes: usize) -> Self {
        Self {
            max_bytes,
            resident_bytes: 0,
            entries: BTreeMap::new(),
            lru: VecDeque::new(),
            hits: 0,
            misses: 0,
        }
    }

    pub(crate) fn stats(&self) -> SstableCacheStats {
        SstableCacheStats {
            max_bytes: self.max_bytes,
            resident_bytes: self.resident_bytes,
            entries: self.entries.len(),
            hits: self.hits,
            misses: self.misses,
        }
    }

    fn get(&mut self, key: CacheKey) -> Option<Arc<[u8]>> {
        let Some(value) = self.entries.get(&key).cloned() else {
            self.misses = self.misses.saturating_add(1);
            return None;
        };
        self.hits = self.hits.saturating_add(1);
        if let Some(position) = self.lru.iter().position(|candidate| *candidate == key) {
            self.lru.remove(position);
        }
        self.lru.push_back(key);
        Some(value)
    }

    fn insert(&mut self, key: CacheKey, value: Arc<[u8]>) {
        let bytes = value.len();
        if self.max_bytes == 0 || bytes > self.max_bytes {
            return;
        }

        if let Some(previous) = self.entries.remove(&key) {
            self.resident_bytes = self.resident_bytes.saturating_sub(previous.len());
            if let Some(position) = self.lru.iter().position(|candidate| *candidate == key) {
                self.lru.remove(position);
            }
        }

        while self.resident_bytes.saturating_add(bytes) > self.max_bytes {
            let Some(oldest) = self.lru.pop_front() else {
                break;
            };
            if let Some(evicted) = self.entries.remove(&oldest) {
                self.resident_bytes = self.resident_bytes.saturating_sub(evicted.len());
            }
        }

        self.resident_bytes = self.resident_bytes.saturating_add(bytes);
        self.entries.insert(key, value);
        self.lru.push_back(key);
    }
}

#[derive(Debug, Clone)]
struct BlockIndexEntry {
    first_key: Vec<u8>,
    last_key: Vec<u8>,
    offset: u64,
    len: u32,
    checksum: [u8; 32],
}

#[derive(Debug, Clone)]
pub(crate) struct Sstable {
    metadata: SstableMetadata,
    path: PathBuf,
    blocks: Vec<BlockIndexEntry>,
    sequence_ranges: Vec<(u64, u64)>,
    cache: Arc<Mutex<SstableBlockCache>>,
}

impl Sstable {
    pub(crate) fn open(path: &Path) -> Result<Self, SstableError> {
        Self::open_with_cache(
            path,
            Arc::new(Mutex::new(SstableBlockCache::new(
                DEFAULT_BLOCK_CACHE_BYTES,
            ))),
        )
    }

    pub(crate) fn open_with_cache(
        path: &Path,
        cache: Arc<Mutex<SstableBlockCache>>,
    ) -> Result<Self, SstableError> {
        let mut file = File::open(path)?;
        let mut magic = [0_u8; 8];
        file.read_exact(&mut magic)?;
        if &magic != SSTABLE_MAGIC {
            return Err(SstableError::InvalidHeader);
        }

        let version = read_u16(&mut file)?;
        if version != SSTABLE_VERSION {
            return Err(SstableError::UnsupportedVersion(version));
        }
        let payload_len = read_u32(&mut file)? as usize;
        if payload_len > MAX_SSTABLE_BYTES {
            return Err(SstableError::TooLarge(payload_len));
        }
        let payload_start = file.stream_position()?;

        let (tablet_id, ownership_epoch, row_count, blocks, sequences, computed_checksum) = {
            let mut reader = PayloadReader::new(&mut file, payload_len);
            let tablet_id = reader.u64()?;
            let ownership_epoch = reader.u64()?;
            let row_count = reader.u32()? as usize;
            if row_count == 0 {
                return Err(SstableError::EmptyTable);
            }
            if row_count > MAX_ROWS {
                return Err(SstableError::TooManyRows(row_count));
            }

            let mut blocks = Vec::new();
            let mut sequences = BTreeSet::new();
            let mut previous_key: Option<Vec<u8>> = None;
            let mut block_start = 0_usize;
            let mut block_first_key: Option<Vec<u8>> = None;
            let mut block_last_key: Option<Vec<u8>> = None;

            for row_index in 0..row_count {
                if block_first_key.is_none() {
                    block_start = reader.consumed();
                    reader.begin_block()?;
                }

                let key = reader.bytes(MAX_KEY_BYTES)?;
                if key.is_empty() {
                    return Err(SstableError::InvalidKey);
                }
                if previous_key
                    .as_deref()
                    .is_some_and(|previous| previous >= key.as_slice())
                {
                    return Err(SstableError::RowsNotStrictlySorted);
                }
                previous_key = Some(key.clone());
                if block_first_key.is_none() {
                    block_first_key = Some(key.clone());
                }
                block_last_key = Some(key);

                let version_count = reader.u32()? as usize;
                if version_count == 0 || version_count > MAX_VERSIONS_PER_ROW {
                    return Err(SstableError::InvalidHistory);
                }
                if version_count > reader.remaining() / 9 {
                    return Err(SstableError::InvalidLength);
                }

                let mut previous_sequence = 0_u64;
                for _ in 0..version_count {
                    let sequence = reader.u64()?;
                    if sequence == 0 || sequence <= previous_sequence {
                        return Err(SstableError::InvalidHistory);
                    }
                    previous_sequence = sequence;
                    sequences.insert(sequence);
                    match reader.u8()? {
                        0 => {}
                        1 => reader.skip_bytes(MAX_VALUE_BYTES)?,
                        other => return Err(SstableError::InvalidValueTag(other)),
                    }
                }

                let block_len = reader.consumed().saturating_sub(block_start);
                if block_len >= TARGET_BLOCK_BYTES || row_index + 1 == row_count {
                    let checksum = reader.finish_block()?;
                    let len = u32::try_from(block_len).map_err(|_| SstableError::InvalidLength)?;
                    blocks.push(BlockIndexEntry {
                        first_key: block_first_key.take().ok_or(SstableError::InvalidHistory)?,
                        last_key: block_last_key.take().ok_or(SstableError::InvalidHistory)?,
                        offset: payload_start
                            .checked_add(block_start as u64)
                            .ok_or(SstableError::InvalidLength)?,
                        len,
                        checksum,
                    });
                }
            }

            if reader.remaining() != 0 {
                return Err(SstableError::TrailingPayloadBytes);
            }
            let computed_checksum = reader.finish_payload()?;
            (
                tablet_id,
                ownership_epoch,
                row_count,
                blocks,
                sequences,
                computed_checksum,
            )
        };

        let mut checksum = [0_u8; 32];
        file.read_exact(&mut checksum)?;
        if checksum != computed_checksum {
            return Err(SstableError::ChecksumMismatch);
        }
        let mut trailing = [0_u8; 1];
        if file.read(&mut trailing)? != 0 {
            return Err(SstableError::TrailingBytes);
        }

        let min_sequence = sequences.iter().next().copied().ok_or(SstableError::InvalidHistory)?;
        let max_sequence = sequences
            .iter()
            .next_back()
            .copied()
            .ok_or(SstableError::InvalidHistory)?;
        let sequence_ranges = compress_sequence_ranges(sequences);
        let min_key = blocks
            .first()
            .map(|block| block.first_key.clone())
            .ok_or(SstableError::EmptyTable)?;
        let max_key = blocks
            .last()
            .map(|block| block.last_key.clone())
            .ok_or(SstableError::EmptyTable)?;
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or(SstableError::InvalidFileName)?
            .to_owned();

        Ok(Self {
            metadata: SstableMetadata {
                tablet_id,
                ownership_epoch,
                min_sequence,
                max_sequence,
                row_count: u32::try_from(row_count)
                    .map_err(|_| SstableError::TooManyRows(row_count))?,
                min_key,
                max_key,
                checksum,
                file_name,
            },
            path: path.to_path_buf(),
            blocks,
            sequence_ranges,
            cache,
        })
    }

    pub(crate) fn tablet_id(&self) -> u64 {
        self.metadata.tablet_id
    }

    pub(crate) fn metadata(&self) -> &SstableMetadata {
        &self.metadata
    }

    pub(crate) fn has_contiguous_sequence_coverage_after(&self, floor: u64) -> bool {
        if self.metadata.max_sequence <= floor {
            return true;
        }
        let Some(mut expected) = floor.checked_add(1) else {
            return false;
        };
        for &(start, end) in &self.sequence_ranges {
            if end < expected {
                continue;
            }
            if start > expected {
                return false;
            }
            if end >= self.metadata.max_sequence {
                return true;
            }
            let Some(next) = end.checked_add(1) else {
                return false;
            };
            expected = next;
        }
        false
    }

    pub(crate) fn version_at(
        &self,
        key: &[u8],
        snapshot: u64,
    ) -> Result<Option<VersionedValue>, SstableError> {
        let Some(block_index) = self.find_block(key) else {
            return Ok(None);
        };
        let bytes = self.read_block(block_index)?;
        let rows = decode_block(&bytes)?;
        let Ok(row_index) = rows.binary_search_by(|row| row.key.as_slice().cmp(key)) else {
            return Ok(None);
        };
        Ok(rows[row_index]
            .versions
            .iter()
            .rev()
            .find(|version| version.sequence <= snapshot)
            .cloned())
    }

    pub(crate) fn get_at(
        &self,
        key: &[u8],
        snapshot: u64,
    ) -> Result<Option<Vec<u8>>, SstableError> {
        Ok(self.version_at(key, snapshot)?.and_then(|version| version.value))
    }

    pub(crate) fn read_all_rows(&self) -> Result<Vec<TabletSnapshotRow>, SstableError> {
        let mut rows = Vec::with_capacity(self.metadata.row_count as usize);
        for block_index in 0..self.blocks.len() {
            let bytes = self.read_block_uncached(block_index)?;
            rows.extend(decode_block(&bytes)?);
        }
        if rows.len() != self.metadata.row_count as usize {
            return Err(SstableError::InvalidHistory);
        }
        validate_rows(&rows)?;
        Ok(rows)
    }

    fn find_block(&self, key: &[u8]) -> Option<usize> {
        let index = match self
            .blocks
            .binary_search_by(|block| block.first_key.as_slice().cmp(key))
        {
            Ok(index) => index,
            Err(0) => return None,
            Err(index) => index - 1,
        };
        (key <= self.blocks[index].last_key.as_slice()).then_some(index)
    }

    fn read_block(&self, block_index: usize) -> Result<Arc<[u8]>, SstableError> {
        let key = (
            self.metadata.checksum,
            u32::try_from(block_index).map_err(|_| SstableError::InvalidLength)?,
        );
        if let Some(bytes) = self
            .cache
            .lock()
            .map_err(|_| SstableError::CachePoisoned)?
            .get(key)
        {
            return Ok(bytes);
        }

        let bytes: Arc<[u8]> = self.read_block_uncached(block_index)?.into();
        self.cache
            .lock()
            .map_err(|_| SstableError::CachePoisoned)?
            .insert(key, Arc::clone(&bytes));
        Ok(bytes)
    }

    fn read_block_uncached(&self, block_index: usize) -> Result<Vec<u8>, SstableError> {
        let block = self
            .blocks
            .get(block_index)
            .ok_or(SstableError::InvalidLength)?;
        let mut file = File::open(&self.path)?;
        file.seek(SeekFrom::Start(block.offset))?;
        let mut bytes = vec![0_u8; block.len as usize];
        file.read_exact(&mut bytes)?;
        if *blake3::hash(&bytes).as_bytes() != block.checksum {
            return Err(SstableError::ChecksumMismatch);
        }
        Ok(bytes)
    }
}

pub(crate) fn expected_metadata(
    file_name: String,
    tablet_id: u64,
    ownership_epoch: u64,
    rows: &[TabletSnapshotRow],
) -> Result<SstableMetadata, SstableError> {
    if rows.is_empty() {
        return Err(SstableError::EmptyTable);
    }
    validate_rows(rows)?;
    let payload = encode_payload(tablet_id, ownership_epoch, rows)?;
    if payload.len() > MAX_SSTABLE_BYTES {
        return Err(SstableError::TooLarge(payload.len()));
    }
    let checksum = *blake3::hash(&payload).as_bytes();
    let min_sequence = rows
        .iter()
        .flat_map(|row| row.versions.iter())
        .map(|version| version.sequence)
        .min()
        .unwrap();
    let max_sequence = rows
        .iter()
        .flat_map(|row| row.versions.iter())
        .map(|version| version.sequence)
        .max()
        .unwrap();
    Ok(SstableMetadata {
        tablet_id,
        ownership_epoch,
        min_sequence,
        max_sequence,
        row_count: u32::try_from(rows.len()).map_err(|_| SstableError::TooManyRows(rows.len()))?,
        min_key: rows.first().unwrap().key.clone(),
        max_key: rows.last().unwrap().key.clone(),
        checksum,
        file_name,
    })
}

pub(crate) fn write_sstable(
    path: &Path,
    tablet_id: u64,
    ownership_epoch: u64,
    rows: &[TabletSnapshotRow],
) -> Result<SstableMetadata, SstableError> {
    if rows.is_empty() {
        return Err(SstableError::EmptyTable);
    }
    validate_rows(rows)?;
    let payload = encode_payload(tablet_id, ownership_epoch, rows)?;
    if payload.len() > MAX_SSTABLE_BYTES {
        return Err(SstableError::TooLarge(payload.len()));
    }
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }
    let temp = appended_path(path, ".tmp");
    let checksum = *blake3::hash(&payload).as_bytes();
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&temp)?;
    file.write_all(SSTABLE_MAGIC)?;
    file.write_all(&SSTABLE_VERSION.to_le_bytes())?;
    file.write_all(&(payload.len() as u32).to_le_bytes())?;
    file.write_all(&payload)?;
    file.write_all(&checksum)?;
    #[cfg(test)]
    super::interruption::hit(super::interruption::StorageInterruptionPoint::SstableAfterTempWrite)?;
    file.sync_data()?;
    #[cfg(test)]
    super::interruption::hit(super::interruption::StorageInterruptionPoint::SstableAfterTempSync)?;
    drop(file);
    fs::rename(&temp, path)?;
    #[cfg(test)]
    super::interruption::hit(super::interruption::StorageInterruptionPoint::SstableAfterRename)?;
    sync_parent_directory(path)?;
    #[cfg(test)]
    super::interruption::hit(
        super::interruption::StorageInterruptionPoint::SstableAfterDirectorySync,
    )?;

    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(SstableError::InvalidFileName)?
        .to_owned();
    expected_metadata(file_name, tablet_id, ownership_epoch, rows)
}

fn encode_payload(
    tablet_id: u64,
    ownership_epoch: u64,
    rows: &[TabletSnapshotRow],
) -> Result<Vec<u8>, SstableError> {
    let mut out = Vec::new();
    out.extend_from_slice(&tablet_id.to_le_bytes());
    out.extend_from_slice(&ownership_epoch.to_le_bytes());
    out.extend_from_slice(&(rows.len() as u32).to_le_bytes());
    for row in rows {
        write_len_prefixed(&mut out, &row.key)?;
        out.extend_from_slice(&(row.versions.len() as u32).to_le_bytes());
        for version in &row.versions {
            out.extend_from_slice(&version.sequence.to_le_bytes());
            match &version.value {
                Some(value) => {
                    out.push(1);
                    write_len_prefixed(&mut out, value)?;
                }
                None => out.push(0),
            }
        }
    }
    Ok(out)
}

fn decode_block(bytes: &[u8]) -> Result<Vec<TabletSnapshotRow>, SstableError> {
    let mut cursor = Cursor::new(bytes);
    let mut rows = Vec::new();
    while cursor.remaining() != 0 {
        let key = cursor.bytes(MAX_KEY_BYTES)?;
        let version_count = cursor.u32()? as usize;
        if version_count == 0 || version_count > MAX_VERSIONS_PER_ROW {
            return Err(SstableError::InvalidHistory);
        }
        if version_count > cursor.remaining() / 9 {
            return Err(SstableError::InvalidLength);
        }
        let mut versions = Vec::with_capacity(version_count);
        for _ in 0..version_count {
            let sequence = cursor.u64()?;
            let value = match cursor.u8()? {
                0 => None,
                1 => Some(cursor.bytes(MAX_VALUE_BYTES)?),
                other => return Err(SstableError::InvalidValueTag(other)),
            };
            versions.push(VersionedValue { sequence, value });
        }
        rows.push(TabletSnapshotRow { key, versions });
    }
    validate_rows(&rows)?;
    Ok(rows)
}

fn compress_sequence_ranges(sequences: BTreeSet<u64>) -> Vec<(u64, u64)> {
    let mut ranges = Vec::new();
    let mut iter = sequences.into_iter();
    let Some(mut start) = iter.next() else {
        return ranges;
    };
    let mut end = start;
    for sequence in iter {
        if end.checked_add(1) == Some(sequence) {
            end = sequence;
        } else {
            ranges.push((start, end));
            start = sequence;
            end = sequence;
        }
    }
    ranges.push((start, end));
    ranges
}

fn validate_rows(rows: &[TabletSnapshotRow]) -> Result<(), SstableError> {
    if rows.is_empty() {
        return Err(SstableError::EmptyTable);
    }
    if rows.len() > MAX_ROWS {
        return Err(SstableError::TooManyRows(rows.len()));
    }
    let mut previous_key: Option<&[u8]> = None;
    for row in rows {
        if row.key.is_empty() || row.key.len() > MAX_KEY_BYTES {
            return Err(SstableError::InvalidKey);
        }
        if previous_key.is_some_and(|key| key >= row.key.as_slice()) {
            return Err(SstableError::RowsNotStrictlySorted);
        }
        previous_key = Some(&row.key);
        if row.versions.is_empty() || row.versions.len() > MAX_VERSIONS_PER_ROW {
            return Err(SstableError::InvalidHistory);
        }
        let mut previous_sequence = 0_u64;
        for version in &row.versions {
            if version.sequence == 0 || version.sequence <= previous_sequence {
                return Err(SstableError::InvalidHistory);
            }
            previous_sequence = version.sequence;
            if version
                .value
                .as_ref()
                .is_some_and(|value| value.len() > MAX_VALUE_BYTES)
            {
                return Err(SstableError::ValueTooLarge);
            }
        }
    }
    Ok(())
}

fn write_len_prefixed(out: &mut Vec<u8>, bytes: &[u8]) -> Result<(), SstableError> {
    let len = u32::try_from(bytes.len()).map_err(|_| SstableError::InvalidLength)?;
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(bytes);
    Ok(())
}

struct PayloadReader<'a> {
    file: &'a mut File,
    remaining: usize,
    consumed: usize,
    payload_hasher: blake3::Hasher,
    block_hasher: Option<blake3::Hasher>,
}

impl<'a> PayloadReader<'a> {
    fn new(file: &'a mut File, payload_len: usize) -> Self {
        Self {
            file,
            remaining: payload_len,
            consumed: 0,
            payload_hasher: blake3::Hasher::new(),
            block_hasher: None,
        }
    }

    fn remaining(&self) -> usize {
        self.remaining
    }

    fn consumed(&self) -> usize {
        self.consumed
    }

    fn begin_block(&mut self) -> Result<(), SstableError> {
        if self.block_hasher.is_some() {
            return Err(SstableError::InvalidHistory);
        }
        self.block_hasher = Some(blake3::Hasher::new());
        Ok(())
    }

    fn finish_block(&mut self) -> Result<[u8; 32], SstableError> {
        let hasher = self.block_hasher.take().ok_or(SstableError::InvalidHistory)?;
        Ok(*hasher.finalize().as_bytes())
    }

    fn finish_payload(self) -> Result<[u8; 32], SstableError> {
        if self.block_hasher.is_some() || self.remaining != 0 {
            return Err(SstableError::InvalidLength);
        }
        Ok(*self.payload_hasher.finalize().as_bytes())
    }

    fn read_exact_into(&mut self, out: &mut [u8]) -> Result<(), SstableError> {
        if out.len() > self.remaining {
            return Err(SstableError::InvalidLength);
        }
        self.file.read_exact(out)?;
        self.payload_hasher.update(out);
        if let Some(hasher) = self.block_hasher.as_mut() {
            hasher.update(out);
        }
        self.remaining -= out.len();
        self.consumed = self.consumed.saturating_add(out.len());
        Ok(())
    }

    fn u8(&mut self) -> Result<u8, SstableError> {
        let mut bytes = [0_u8; 1];
        self.read_exact_into(&mut bytes)?;
        Ok(bytes[0])
    }

    fn u32(&mut self) -> Result<u32, SstableError> {
        let mut bytes = [0_u8; 4];
        self.read_exact_into(&mut bytes)?;
        Ok(u32::from_le_bytes(bytes))
    }

    fn u64(&mut self) -> Result<u64, SstableError> {
        let mut bytes = [0_u8; 8];
        self.read_exact_into(&mut bytes)?;
        Ok(u64::from_le_bytes(bytes))
    }

    fn bytes(&mut self, max: usize) -> Result<Vec<u8>, SstableError> {
        let len = self.u32()? as usize;
        if len > max || len > self.remaining {
            return Err(SstableError::InvalidLength);
        }
        let mut value = vec![0_u8; len];
        self.read_exact_into(&mut value)?;
        Ok(value)
    }

    fn skip_bytes(&mut self, max: usize) -> Result<(), SstableError> {
        let len = self.u32()? as usize;
        if len > max || len > self.remaining {
            return Err(SstableError::InvalidLength);
        }
        let mut remaining = len;
        let mut scratch = [0_u8; 8192];
        while remaining != 0 {
            let take = remaining.min(scratch.len());
            self.read_exact_into(&mut scratch[..take])?;
            remaining -= take;
        }
        Ok(())
    }
}

struct Cursor<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.offset)
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], SstableError> {
        let end = self
            .offset
            .checked_add(len)
            .ok_or(SstableError::InvalidLength)?;
        if end > self.bytes.len() {
            return Err(SstableError::InvalidLength);
        }
        let value = &self.bytes[self.offset..end];
        self.offset = end;
        Ok(value)
    }

    fn u8(&mut self) -> Result<u8, SstableError> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32, SstableError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn u64(&mut self) -> Result<u64, SstableError> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    fn bytes(&mut self, max: usize) -> Result<Vec<u8>, SstableError> {
        let len = self.u32()? as usize;
        if len > max || len > self.remaining() {
            return Err(SstableError::InvalidLength);
        }
        Ok(self.take(len)?.to_vec())
    }
}

fn read_u16(file: &mut File) -> io::Result<u16> {
    let mut bytes = [0_u8; 2];
    file.read_exact(&mut bytes)?;
    Ok(u16::from_le_bytes(bytes))
}

fn read_u32(file: &mut File) -> io::Result<u32> {
    let mut bytes = [0_u8; 4];
    file.read_exact(&mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

fn appended_path(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

fn sync_parent_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        File::open(parent)?.sync_all()?;
    }

    #[cfg(not(unix))]
    let _ = path;

    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SstableError {
    Io {
        kind: io::ErrorKind,
        message: String,
    },
    InvalidHeader,
    UnsupportedVersion(u16),
    TooLarge(usize),
    ChecksumMismatch,
    TrailingBytes,
    TrailingPayloadBytes,
    InvalidFileName,
    EmptyTable,
    TooManyRows(usize),
    InvalidKey,
    RowsNotStrictlySorted,
    InvalidHistory,
    InvalidValueTag(u8),
    ValueTooLarge,
    InvalidLength,
    ExistingFileMismatch(String),
    CachePoisoned,
}

impl From<io::Error> for SstableError {
    fn from(error: io::Error) -> Self {
        Self::Io {
            kind: error.kind(),
            message: error.to_string(),
        }
    }
}

impl fmt::Display for SstableError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "NuDB SSTable error: {self:?}")
    }
}

impl std::error::Error for SstableError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEST: AtomicU64 = AtomicU64::new(1);

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "nulang_nudb_sstable_{name}_{}_{}.sst",
            std::process::id(),
            NEXT_TEST.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn rows() -> Vec<TabletSnapshotRow> {
        vec![
            TabletSnapshotRow {
                key: b"alpha".to_vec(),
                versions: vec![
                    VersionedValue {
                        sequence: 1,
                        value: Some(b"one".to_vec()),
                    },
                    VersionedValue {
                        sequence: 3,
                        value: Some(b"three".to_vec()),
                    },
                ],
            },
            TabletSnapshotRow {
                key: b"beta".to_vec(),
                versions: vec![
                    VersionedValue {
                        sequence: 2,
                        value: Some(b"two".to_vec()),
                    },
                    VersionedValue {
                        sequence: 4,
                        value: None,
                    },
                ],
            },
        ]
    }

    #[test]
    fn write_open_and_mvcc_lookup_round_trip() {
        let path = temp_path("roundtrip");
        let _ = fs::remove_file(&path);
        let metadata = write_sstable(&path, 42, 7, &rows()).unwrap();
        assert_eq!(metadata.min_sequence, 1);
        assert_eq!(metadata.max_sequence, 4);
        assert_eq!(metadata.row_count, 2);
        let table = Sstable::open(&path).unwrap();
        assert_eq!(table.tablet_id(), 42);
        assert_eq!(table.get_at(b"alpha", 1).unwrap().as_deref(), Some(&b"one"[..]));
        assert_eq!(
            table.get_at(b"alpha", 3).unwrap().as_deref(),
            Some(&b"three"[..])
        );
        assert_eq!(table.get_at(b"beta", 3).unwrap().as_deref(), Some(&b"two"[..]));
        assert_eq!(table.get_at(b"beta", 4).unwrap(), None);
        assert_eq!(table.get_at(b"missing", 4).unwrap(), None);
        assert_eq!(table.read_all_rows().unwrap(), rows());
        let _ = fs::remove_file(path);
    }

    #[test]
    fn complete_checksum_corruption_fails_closed() {
        let path = temp_path("corrupt");
        let _ = fs::remove_file(&path);
        write_sstable(&path, 42, 7, &rows()).unwrap();
        let mut bytes = fs::read(&path).unwrap();
        bytes[16] ^= 0x40;
        fs::write(&path, bytes).unwrap();
        assert_eq!(
            Sstable::open(&path).unwrap_err(),
            SstableError::ChecksumMismatch
        );
        let _ = fs::remove_file(path);
    }

    #[test]
    fn truncated_sstable_fails_closed() {
        let path = temp_path("truncated");
        let _ = fs::remove_file(&path);
        write_sstable(&path, 42, 7, &rows()).unwrap();
        let mut bytes = fs::read(&path).unwrap();
        bytes.truncate(bytes.len() - 9);
        fs::write(&path, bytes).unwrap();
        assert!(matches!(
            Sstable::open(&path),
            Err(SstableError::Io { .. }) | Err(SstableError::InvalidLength)
        ));
        let _ = fs::remove_file(path);
    }
}
