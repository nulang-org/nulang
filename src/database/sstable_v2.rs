//! NuDB SSTable v2 codec with independently checksummed data blocks.
//!
//! V2 is intentionally separate from the serving path while its format contract
//! stabilizes. Opening a table reads and validates only the fixed header, trailer,
//! and checksummed footer/index. Data blocks are read and checksummed lazily on
//! point lookup, so corruption in an untouched block does not force a full-table
//! scan at open.

use std::collections::BTreeSet;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
#[cfg(not(unix))]
use std::io::{Read, Seek, SeekFrom};
use std::ops::Range;
#[cfg(unix)]
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

use super::tablet::{TabletSnapshotRow, VersionedValue};

const SSTABLE_MAGIC: &[u8; 8] = b"NUDBSST2";
const SSTABLE_VERSION: u16 = 2;
const FOOTER_MAGIC: &[u8; 8] = b"NUDBIDX2";
const TRAILER_MAGIC: &[u8; 8] = b"NUDBEND2";
const HEADER_BYTES: usize = 8 + 2;
const TRAILER_BYTES: usize = 4 + 32 + 8;
const TARGET_BLOCK_BYTES: usize = 16 * 1024;
const MAX_SSTABLE_BYTES: usize = 256 * 1024 * 1024;
const MAX_FOOTER_BYTES: usize = 64 * 1024 * 1024;
const MAX_FILE_BYTES: usize = MAX_SSTABLE_BYTES + MAX_FOOTER_BYTES + TRAILER_BYTES + HEADER_BYTES;
const MAX_ROWS: usize = 4_000_000;
const MAX_BLOCKS: usize = 1_000_000;
const MAX_SEQUENCE_RUNS: usize = 1_000_000;
const MAX_VERSIONS_PER_ROW: usize = 65_536;
const MAX_KEY_BYTES: usize = 16 * 1024 * 1024;
const MAX_VALUE_BYTES: usize = 64 * 1024 * 1024;
const MAX_BLOCK_BYTES: usize = 128 * 1024 * 1024;
const MIN_BLOCK_INDEX_BYTES: usize = 8 + 4 + 4 + 8 + 8 + 4 + 1 + 4 + 1 + 32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SstableV2Metadata {
    pub(crate) tablet_id: u64,
    pub(crate) ownership_epoch: u64,
    pub(crate) min_sequence: u64,
    pub(crate) max_sequence: u64,
    pub(crate) row_count: u32,
    pub(crate) min_key: Vec<u8>,
    pub(crate) max_key: Vec<u8>,
    pub(crate) block_count: u32,
    pub(crate) footer_checksum: [u8; 32],
    pub(crate) file_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OwnedVersion {
    pub(crate) sequence: u64,
    pub(crate) value: Option<Vec<u8>>,
}

#[derive(Debug, Clone)]
struct BlockIndex {
    payload_offset: u64,
    payload_len: u32,
    row_count: u32,
    min_sequence: u64,
    max_sequence: u64,
    first_key: Vec<u8>,
    last_key: Vec<u8>,
    checksum: [u8; 32],
}

#[derive(Debug, Clone, Copy)]
struct SequenceRun {
    start: u64,
    end: u64,
}

#[derive(Debug)]
pub(crate) struct SstableV2 {
    file: File,
    metadata: SstableV2Metadata,
    blocks: Vec<BlockIndex>,
    sequence_runs: Vec<SequenceRun>,
    footer_start: u64,
    footer_len: usize,
}

impl SstableV2 {
    pub(crate) fn open(path: &Path) -> Result<Self, SstableV2Error> {
        let file = File::open(path)?;
        let file_len = file.metadata()?.len();
        if file_len > MAX_FILE_BYTES as u64 {
            return Err(SstableV2Error::TooLarge(
                usize::try_from(file_len).unwrap_or(usize::MAX),
            ));
        }
        if file_len < (HEADER_BYTES + TRAILER_BYTES) as u64 {
            return Err(SstableV2Error::InvalidLength);
        }

        let mut header = [0_u8; HEADER_BYTES];
        read_exact_at(&file, 0, &mut header)?;
        if &header[..8] != SSTABLE_MAGIC {
            return Err(SstableV2Error::InvalidHeader);
        }
        let version = u16::from_le_bytes(header[8..10].try_into().unwrap());
        if version != SSTABLE_VERSION {
            return Err(SstableV2Error::UnsupportedVersion(version));
        }

        let trailer_offset = file_len
            .checked_sub(TRAILER_BYTES as u64)
            .ok_or(SstableV2Error::InvalidLength)?;
        let mut trailer = [0_u8; TRAILER_BYTES];
        read_exact_at(&file, trailer_offset, &mut trailer)?;
        if &trailer[36..44] != TRAILER_MAGIC {
            return Err(SstableV2Error::InvalidFooter);
        }
        let footer_len = u32::from_le_bytes(trailer[..4].try_into().unwrap()) as usize;
        if footer_len == 0 || footer_len > MAX_FOOTER_BYTES {
            return Err(SstableV2Error::InvalidFooter);
        }
        let footer_start = trailer_offset
            .checked_sub(footer_len as u64)
            .ok_or(SstableV2Error::InvalidLength)?;
        if footer_start < HEADER_BYTES as u64 {
            return Err(SstableV2Error::InvalidLength);
        }

        let mut footer = vec![0_u8; footer_len];
        read_exact_at(&file, footer_start, &mut footer)?;
        let mut expected_footer_checksum = [0_u8; 32];
        expected_footer_checksum.copy_from_slice(&trailer[4..36]);
        if expected_footer_checksum != *blake3::hash(&footer).as_bytes() {
            return Err(SstableV2Error::FooterChecksumMismatch);
        }

        let decoded = decode_footer(&footer, footer_start)?;
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or(SstableV2Error::InvalidFileName)?
            .to_owned();
        let block_count = u32::try_from(decoded.blocks.len())
            .map_err(|_| SstableV2Error::TooManyBlocks(decoded.blocks.len()))?;

        Ok(Self {
            file,
            metadata: SstableV2Metadata {
                tablet_id: decoded.tablet_id,
                ownership_epoch: decoded.ownership_epoch,
                min_sequence: decoded.min_sequence,
                max_sequence: decoded.max_sequence,
                row_count: decoded.row_count,
                min_key: decoded.min_key,
                max_key: decoded.max_key,
                block_count,
                footer_checksum: expected_footer_checksum,
                file_name,
            },
            blocks: decoded.blocks,
            sequence_runs: decoded.sequence_runs,
            footer_start,
            footer_len,
        })
    }

    pub(crate) fn metadata(&self) -> &SstableV2Metadata {
        &self.metadata
    }

    pub(crate) fn version_at(
        &self,
        key: &[u8],
        snapshot: u64,
    ) -> Result<Option<OwnedVersion>, SstableV2Error> {
        if snapshot < self.metadata.min_sequence
            || key < self.metadata.min_key.as_slice()
            || key > self.metadata.max_key.as_slice()
        {
            return Ok(None);
        }

        let insertion = self
            .blocks
            .partition_point(|block| block.first_key.as_slice() <= key);
        let Some(block_index) = insertion.checked_sub(1) else {
            return Ok(None);
        };
        let block = &self.blocks[block_index];
        if key > block.last_key.as_slice() || snapshot < block.min_sequence {
            return Ok(None);
        }

        let mut framed_len = [0_u8; 4];
        let frame_offset = block
            .payload_offset
            .checked_sub(4)
            .ok_or(SstableV2Error::InvalidBlock(block_index))?;
        read_exact_at(&self.file, frame_offset, &mut framed_len)?;
        if u32::from_le_bytes(framed_len) != block.payload_len {
            return Err(SstableV2Error::InvalidBlock(block_index));
        }

        let payload_len = block.payload_len as usize;
        let mut payload = vec![0_u8; payload_len];
        read_exact_at(&self.file, block.payload_offset, &mut payload)?;
        if block.checksum != *blake3::hash(&payload).as_bytes() {
            return Err(SstableV2Error::BlockChecksumMismatch(block_index));
        }

        decode_block_lookup(&payload, block, block_index, key, snapshot)
    }

    pub(crate) fn has_contiguous_sequence_coverage_after(&self, floor: u64) -> bool {
        if self.metadata.max_sequence <= floor {
            return true;
        }
        let Some(required) = floor.checked_add(1) else {
            return false;
        };
        self.sequence_runs.last().is_some_and(|run| {
            run.start <= required && required <= run.end && run.end == self.metadata.max_sequence
        })
    }

    #[cfg(test)]
    pub(crate) fn block_count_for_test(&self) -> usize {
        self.blocks.len()
    }

    #[cfg(test)]
    pub(crate) fn block_payload_range_for_test(&self, index: usize) -> Option<Range<usize>> {
        let block = self.blocks.get(index)?;
        let start = usize::try_from(block.payload_offset).ok()?;
        Some(start..start.checked_add(block.payload_len as usize)?)
    }

    #[cfg(test)]
    pub(crate) fn footer_range_for_test(&self) -> Range<usize> {
        let start = self.footer_start as usize;
        start..start + self.footer_len
    }
}

pub(crate) fn write_sstable_v2(
    path: &Path,
    tablet_id: u64,
    ownership_epoch: u64,
    rows: &[TabletSnapshotRow],
) -> Result<SstableV2Metadata, SstableV2Error> {
    validate_rows(rows)?;
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }

    let temp = appended_path(path, ".tmp-v2");
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&temp)?;
    file.write_all(SSTABLE_MAGIC)?;
    file.write_all(&SSTABLE_VERSION.to_le_bytes())?;

    let mut offset = HEADER_BYTES as u64;
    let mut blocks = Vec::new();
    let mut pending = PendingBlock::default();

    for row in rows {
        let encoded = encode_row(row)?;
        if encoded.len() > MAX_BLOCK_BYTES {
            return Err(SstableV2Error::TooLarge(encoded.len()));
        }
        if pending.row_count != 0
            && pending.payload.len().saturating_add(encoded.len()) > TARGET_BLOCK_BYTES
        {
            flush_pending_block(&mut file, &mut offset, &mut blocks, &mut pending)?;
        }
        pending.push(row, &encoded);
    }
    flush_pending_block(&mut file, &mut offset, &mut blocks, &mut pending)?;

    if blocks.is_empty() || blocks.len() > MAX_BLOCKS {
        return Err(SstableV2Error::TooManyBlocks(blocks.len()));
    }
    if offset.saturating_sub(HEADER_BYTES as u64) > MAX_SSTABLE_BYTES as u64 {
        return Err(SstableV2Error::TooLarge(offset as usize));
    }

    let sequence_runs = sequence_runs(rows)?;
    let footer = encode_footer(
        tablet_id,
        ownership_epoch,
        rows.len(),
        &blocks,
        &sequence_runs,
    )?;
    if footer.len() > MAX_FOOTER_BYTES {
        return Err(SstableV2Error::TooLarge(footer.len()));
    }
    let footer_checksum = *blake3::hash(&footer).as_bytes();
    let footer_len = u32::try_from(footer.len()).map_err(|_| SstableV2Error::InvalidLength)?;

    file.write_all(&footer)?;
    file.write_all(&footer_len.to_le_bytes())?;
    file.write_all(&footer_checksum)?;
    file.write_all(TRAILER_MAGIC)?;
    file.sync_data()?;
    drop(file);
    fs::rename(&temp, path)?;
    sync_parent_directory(path)?;

    let min_sequence = sequence_runs
        .first()
        .map(|run| run.start)
        .ok_or(SstableV2Error::InvalidHistory)?;
    let max_sequence = sequence_runs
        .last()
        .map(|run| run.end)
        .ok_or(SstableV2Error::InvalidHistory)?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(SstableV2Error::InvalidFileName)?
        .to_owned();

    Ok(SstableV2Metadata {
        tablet_id,
        ownership_epoch,
        min_sequence,
        max_sequence,
        row_count: u32::try_from(rows.len())
            .map_err(|_| SstableV2Error::TooManyRows(rows.len()))?,
        min_key: rows.first().unwrap().key.clone(),
        max_key: rows.last().unwrap().key.clone(),
        block_count: u32::try_from(blocks.len())
            .map_err(|_| SstableV2Error::TooManyBlocks(blocks.len()))?,
        footer_checksum,
        file_name,
    })
}

#[derive(Default)]
struct PendingBlock {
    payload: Vec<u8>,
    row_count: u32,
    min_sequence: u64,
    max_sequence: u64,
    first_key: Vec<u8>,
    last_key: Vec<u8>,
}

impl PendingBlock {
    fn push(&mut self, row: &TabletSnapshotRow, encoded: &[u8]) {
        if self.row_count == 0 {
            self.first_key = row.key.clone();
            self.min_sequence = u64::MAX;
        }
        self.last_key.clear();
        self.last_key.extend_from_slice(&row.key);
        for version in &row.versions {
            self.min_sequence = self.min_sequence.min(version.sequence);
            self.max_sequence = self.max_sequence.max(version.sequence);
        }
        self.row_count += 1;
        self.payload.extend_from_slice(encoded);
    }

    fn reset(&mut self) {
        self.payload.clear();
        self.row_count = 0;
        self.min_sequence = 0;
        self.max_sequence = 0;
        self.first_key.clear();
        self.last_key.clear();
    }
}

fn flush_pending_block(
    file: &mut File,
    offset: &mut u64,
    blocks: &mut Vec<BlockIndex>,
    pending: &mut PendingBlock,
) -> Result<(), SstableV2Error> {
    if pending.row_count == 0 {
        return Ok(());
    }
    if blocks.len() >= MAX_BLOCKS {
        return Err(SstableV2Error::TooManyBlocks(blocks.len() + 1));
    }
    let payload_len = u32::try_from(pending.payload.len())
        .map_err(|_| SstableV2Error::TooLarge(pending.payload.len()))?;
    let payload_offset = offset.checked_add(4).ok_or(SstableV2Error::InvalidLength)?;
    file.write_all(&payload_len.to_le_bytes())?;
    file.write_all(&pending.payload)?;
    let checksum = *blake3::hash(&pending.payload).as_bytes();
    blocks.push(BlockIndex {
        payload_offset,
        payload_len,
        row_count: pending.row_count,
        min_sequence: pending.min_sequence,
        max_sequence: pending.max_sequence,
        first_key: pending.first_key.clone(),
        last_key: pending.last_key.clone(),
        checksum,
    });
    *offset = payload_offset
        .checked_add(payload_len as u64)
        .ok_or(SstableV2Error::InvalidLength)?;
    pending.reset();
    Ok(())
}

fn encode_footer(
    tablet_id: u64,
    ownership_epoch: u64,
    row_count: usize,
    blocks: &[BlockIndex],
    sequence_runs: &[SequenceRun],
) -> Result<Vec<u8>, SstableV2Error> {
    let mut out = Vec::new();
    out.extend_from_slice(FOOTER_MAGIC);
    out.extend_from_slice(&tablet_id.to_le_bytes());
    out.extend_from_slice(&ownership_epoch.to_le_bytes());
    out.extend_from_slice(
        &u32::try_from(row_count)
            .map_err(|_| SstableV2Error::TooManyRows(row_count))?
            .to_le_bytes(),
    );
    out.extend_from_slice(
        &u32::try_from(blocks.len())
            .map_err(|_| SstableV2Error::TooManyBlocks(blocks.len()))?
            .to_le_bytes(),
    );
    out.extend_from_slice(
        &u32::try_from(sequence_runs.len())
            .map_err(|_| SstableV2Error::TooManySequenceRuns(sequence_runs.len()))?
            .to_le_bytes(),
    );

    for block in blocks {
        out.extend_from_slice(&block.payload_offset.to_le_bytes());
        out.extend_from_slice(&block.payload_len.to_le_bytes());
        out.extend_from_slice(&block.row_count.to_le_bytes());
        out.extend_from_slice(&block.min_sequence.to_le_bytes());
        out.extend_from_slice(&block.max_sequence.to_le_bytes());
        write_len_prefixed(&mut out, &block.first_key)?;
        write_len_prefixed(&mut out, &block.last_key)?;
        out.extend_from_slice(&block.checksum);
    }
    for run in sequence_runs {
        out.extend_from_slice(&run.start.to_le_bytes());
        out.extend_from_slice(&run.end.to_le_bytes());
    }
    Ok(out)
}

struct DecodedFooter {
    tablet_id: u64,
    ownership_epoch: u64,
    row_count: u32,
    min_sequence: u64,
    max_sequence: u64,
    min_key: Vec<u8>,
    max_key: Vec<u8>,
    blocks: Vec<BlockIndex>,
    sequence_runs: Vec<SequenceRun>,
}

fn decode_footer(footer: &[u8], footer_start: u64) -> Result<DecodedFooter, SstableV2Error> {
    let mut cursor = Cursor::new(footer);
    if cursor.take(8)? != FOOTER_MAGIC {
        return Err(SstableV2Error::InvalidFooter);
    }
    let tablet_id = cursor.u64()?;
    let ownership_epoch = cursor.u64()?;
    let row_count = cursor.u32()?;
    if row_count == 0 || row_count as usize > MAX_ROWS {
        return Err(SstableV2Error::TooManyRows(row_count as usize));
    }
    let block_count = cursor.u32()? as usize;
    if block_count == 0 || block_count > MAX_BLOCKS {
        return Err(SstableV2Error::TooManyBlocks(block_count));
    }
    let sequence_run_count = cursor.u32()? as usize;
    if sequence_run_count == 0 || sequence_run_count > MAX_SEQUENCE_RUNS {
        return Err(SstableV2Error::TooManySequenceRuns(sequence_run_count));
    }
    if block_count > cursor.remaining() / MIN_BLOCK_INDEX_BYTES {
        return Err(SstableV2Error::InvalidFooter);
    }

    let mut blocks = Vec::with_capacity(block_count);
    let mut expected_frame_start = HEADER_BYTES as u64;
    let mut total_rows = 0_u64;
    let mut previous_last_key: Option<Vec<u8>> = None;
    let mut min_sequence = u64::MAX;
    let mut max_sequence = 0_u64;

    for _ in 0..block_count {
        let payload_offset = cursor.u64()?;
        let payload_len = cursor.u32()?;
        let block_rows = cursor.u32()?;
        let block_min_sequence = cursor.u64()?;
        let block_max_sequence = cursor.u64()?;
        let first_key = cursor.bytes(MAX_KEY_BYTES)?;
        let last_key = cursor.bytes(MAX_KEY_BYTES)?;
        let mut checksum = [0_u8; 32];
        checksum.copy_from_slice(cursor.take(32)?);

        if payload_len == 0 || payload_len as usize > MAX_BLOCK_BYTES || block_rows == 0 {
            return Err(SstableV2Error::InvalidFooter);
        }
        if first_key.is_empty() || last_key.is_empty() || first_key > last_key {
            return Err(SstableV2Error::InvalidFooter);
        }
        if previous_last_key
            .as_ref()
            .is_some_and(|previous| previous.as_slice() >= first_key.as_slice())
        {
            return Err(SstableV2Error::InvalidFooter);
        }
        if block_min_sequence == 0 || block_min_sequence > block_max_sequence {
            return Err(SstableV2Error::InvalidFooter);
        }
        let expected_payload_offset = expected_frame_start
            .checked_add(4)
            .ok_or(SstableV2Error::InvalidLength)?;
        if payload_offset != expected_payload_offset {
            return Err(SstableV2Error::InvalidFooter);
        }
        let block_end = payload_offset
            .checked_add(payload_len as u64)
            .ok_or(SstableV2Error::InvalidLength)?;
        if block_end > footer_start {
            return Err(SstableV2Error::InvalidFooter);
        }
        expected_frame_start = block_end;
        total_rows = total_rows
            .checked_add(block_rows as u64)
            .ok_or(SstableV2Error::InvalidFooter)?;
        previous_last_key = Some(last_key.clone());
        min_sequence = min_sequence.min(block_min_sequence);
        max_sequence = max_sequence.max(block_max_sequence);

        blocks.push(BlockIndex {
            payload_offset,
            payload_len,
            row_count: block_rows,
            min_sequence: block_min_sequence,
            max_sequence: block_max_sequence,
            first_key,
            last_key,
            checksum,
        });
    }

    if expected_frame_start != footer_start || total_rows != row_count as u64 {
        return Err(SstableV2Error::InvalidFooter);
    }
    if sequence_run_count > cursor.remaining() / 16 {
        return Err(SstableV2Error::InvalidFooter);
    }

    let mut sequence_runs = Vec::with_capacity(sequence_run_count);
    let mut previous_run: Option<SequenceRun> = None;
    for _ in 0..sequence_run_count {
        let run = SequenceRun {
            start: cursor.u64()?,
            end: cursor.u64()?,
        };
        if run.start == 0 || run.start > run.end {
            return Err(SstableV2Error::InvalidFooter);
        }
        if let Some(previous) = previous_run {
            let first_after_previous = previous.end.checked_add(1).unwrap_or(u64::MAX);
            if run.start <= first_after_previous {
                return Err(SstableV2Error::InvalidFooter);
            }
        }
        previous_run = Some(run);
        sequence_runs.push(run);
    }
    if cursor.remaining() != 0 {
        return Err(SstableV2Error::InvalidFooter);
    }
    if sequence_runs.first().map(|run| run.start) != Some(min_sequence)
        || sequence_runs.last().map(|run| run.end) != Some(max_sequence)
    {
        return Err(SstableV2Error::InvalidFooter);
    }

    let min_key = blocks
        .first()
        .map(|block| block.first_key.clone())
        .ok_or(SstableV2Error::InvalidFooter)?;
    let max_key = blocks
        .last()
        .map(|block| block.last_key.clone())
        .ok_or(SstableV2Error::InvalidFooter)?;

    Ok(DecodedFooter {
        tablet_id,
        ownership_epoch,
        row_count,
        min_sequence,
        max_sequence,
        min_key,
        max_key,
        blocks,
        sequence_runs,
    })
}

fn decode_block_lookup(
    payload: &[u8],
    block: &BlockIndex,
    block_index: usize,
    key: &[u8],
    snapshot: u64,
) -> Result<Option<OwnedVersion>, SstableV2Error> {
    let mut cursor = Cursor::new(payload);
    let mut previous_key: Option<Vec<u8>> = None;
    let mut first_key = Vec::new();
    let mut last_key = Vec::new();
    let mut min_sequence = u64::MAX;
    let mut max_sequence = 0_u64;
    let mut result = None;

    for row_index in 0..block.row_count {
        let row_key = cursor.bytes(MAX_KEY_BYTES)?;
        if row_key.is_empty() {
            return Err(SstableV2Error::InvalidBlock(block_index));
        }
        if previous_key
            .as_ref()
            .is_some_and(|previous| previous.as_slice() >= row_key.as_slice())
        {
            return Err(SstableV2Error::InvalidBlock(block_index));
        }
        if row_index == 0 {
            first_key = row_key.clone();
        }
        last_key.clone_from(&row_key);
        previous_key = Some(row_key.clone());

        let version_count = cursor.u32()? as usize;
        if version_count == 0 || version_count > MAX_VERSIONS_PER_ROW {
            return Err(SstableV2Error::InvalidBlock(block_index));
        }
        if version_count > cursor.remaining() / 9 {
            return Err(SstableV2Error::InvalidBlock(block_index));
        }
        let mut previous_sequence = 0_u64;
        let mut visible = None;
        for _ in 0..version_count {
            let version = read_version(&mut cursor)?;
            if version.sequence == 0 || version.sequence <= previous_sequence {
                return Err(SstableV2Error::InvalidBlock(block_index));
            }
            previous_sequence = version.sequence;
            min_sequence = min_sequence.min(version.sequence);
            max_sequence = max_sequence.max(version.sequence);
            if row_key.as_slice() == key && version.sequence <= snapshot {
                visible = Some(OwnedVersion {
                    sequence: version.sequence,
                    value: version.value.map(ToOwned::to_owned),
                });
            }
        }
        if row_key.as_slice() == key {
            result = visible;
        }
    }

    if cursor.remaining() != 0
        || first_key != block.first_key
        || last_key != block.last_key
        || min_sequence != block.min_sequence
        || max_sequence != block.max_sequence
    {
        return Err(SstableV2Error::InvalidBlock(block_index));
    }
    Ok(result)
}

struct BorrowedVersion<'a> {
    sequence: u64,
    value: Option<&'a [u8]>,
}

fn read_version<'a>(cursor: &mut Cursor<'a>) -> Result<BorrowedVersion<'a>, SstableV2Error> {
    let sequence = cursor.u64()?;
    let tag = cursor.u8()?;
    let value = match tag {
        0 => None,
        1 => {
            let len = cursor.u32()? as usize;
            if len > MAX_VALUE_BYTES || len > cursor.remaining() {
                return Err(SstableV2Error::InvalidLength);
            }
            Some(cursor.take(len)?)
        }
        other => return Err(SstableV2Error::InvalidValueTag(other)),
    };
    Ok(BorrowedVersion { sequence, value })
}

fn validate_rows(rows: &[TabletSnapshotRow]) -> Result<(), SstableV2Error> {
    if rows.is_empty() {
        return Err(SstableV2Error::EmptyTable);
    }
    if rows.len() > MAX_ROWS {
        return Err(SstableV2Error::TooManyRows(rows.len()));
    }
    let mut previous_key: Option<&[u8]> = None;
    for row in rows {
        if row.key.is_empty() || row.key.len() > MAX_KEY_BYTES {
            return Err(SstableV2Error::InvalidKey);
        }
        if previous_key.is_some_and(|key| key >= row.key.as_slice()) {
            return Err(SstableV2Error::RowsNotStrictlySorted);
        }
        previous_key = Some(&row.key);
        if row.versions.is_empty() || row.versions.len() > MAX_VERSIONS_PER_ROW {
            return Err(SstableV2Error::InvalidHistory);
        }
        let mut previous_sequence = 0_u64;
        for version in &row.versions {
            if version.sequence == 0 || version.sequence <= previous_sequence {
                return Err(SstableV2Error::InvalidHistory);
            }
            previous_sequence = version.sequence;
            if version
                .value
                .as_ref()
                .is_some_and(|value| value.len() > MAX_VALUE_BYTES)
            {
                return Err(SstableV2Error::ValueTooLarge);
            }
        }
    }
    Ok(())
}

fn encode_row(row: &TabletSnapshotRow) -> Result<Vec<u8>, SstableV2Error> {
    let mut out = Vec::new();
    write_len_prefixed(&mut out, &row.key)?;
    out.extend_from_slice(
        &u32::try_from(row.versions.len())
            .map_err(|_| SstableV2Error::InvalidHistory)?
            .to_le_bytes(),
    );
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
    Ok(out)
}

fn sequence_runs(rows: &[TabletSnapshotRow]) -> Result<Vec<SequenceRun>, SstableV2Error> {
    let mut sequences = BTreeSet::new();
    for row in rows {
        for version in &row.versions {
            sequences.insert(version.sequence);
        }
    }
    let mut runs: Vec<SequenceRun> = Vec::new();
    for sequence in sequences {
        match runs.last_mut() {
            Some(run) if run.end.checked_add(1) == Some(sequence) => run.end = sequence,
            _ => runs.push(SequenceRun {
                start: sequence,
                end: sequence,
            }),
        }
        if runs.len() > MAX_SEQUENCE_RUNS {
            return Err(SstableV2Error::TooManySequenceRuns(runs.len()));
        }
    }
    if runs.is_empty() {
        return Err(SstableV2Error::InvalidHistory);
    }
    Ok(runs)
}

fn write_len_prefixed(out: &mut Vec<u8>, bytes: &[u8]) -> Result<(), SstableV2Error> {
    let len = u32::try_from(bytes.len()).map_err(|_| SstableV2Error::InvalidLength)?;
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(bytes);
    Ok(())
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

    fn take(&mut self, len: usize) -> Result<&'a [u8], SstableV2Error> {
        let end = self
            .offset
            .checked_add(len)
            .ok_or(SstableV2Error::InvalidLength)?;
        if end > self.bytes.len() {
            return Err(SstableV2Error::InvalidLength);
        }
        let value = &self.bytes[self.offset..end];
        self.offset = end;
        Ok(value)
    }

    fn u8(&mut self) -> Result<u8, SstableV2Error> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32, SstableV2Error> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn u64(&mut self) -> Result<u64, SstableV2Error> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    fn bytes(&mut self, max: usize) -> Result<Vec<u8>, SstableV2Error> {
        let len = self.u32()? as usize;
        if len > max || len > self.remaining() {
            return Err(SstableV2Error::InvalidLength);
        }
        Ok(self.take(len)?.to_vec())
    }
}

#[cfg(unix)]
fn read_exact_at(file: &File, offset: u64, buf: &mut [u8]) -> io::Result<()> {
    let mut filled = 0_usize;
    while filled < buf.len() {
        let read = file.read_at(&mut buf[filled..], offset + filled as u64)?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "short NuDB SSTable v2 read",
            ));
        }
        filled += read;
    }
    Ok(())
}

#[cfg(not(unix))]
fn read_exact_at(file: &File, offset: u64, buf: &mut [u8]) -> io::Result<()> {
    let mut clone = file.try_clone()?;
    clone.seek(SeekFrom::Start(offset))?;
    clone.read_exact(buf)
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
pub(crate) enum SstableV2Error {
    Io {
        kind: io::ErrorKind,
        message: String,
    },
    InvalidHeader,
    UnsupportedVersion(u16),
    InvalidLength,
    InvalidFooter,
    FooterChecksumMismatch,
    BlockChecksumMismatch(usize),
    InvalidBlock(usize),
    InvalidFileName,
    EmptyTable,
    TooLarge(usize),
    TooManyRows(usize),
    TooManyBlocks(usize),
    TooManySequenceRuns(usize),
    InvalidKey,
    RowsNotStrictlySorted,
    InvalidHistory,
    InvalidValueTag(u8),
    ValueTooLarge,
}

impl From<io::Error> for SstableV2Error {
    fn from(error: io::Error) -> Self {
        Self::Io {
            kind: error.kind(),
            message: error.to_string(),
        }
    }
}

impl fmt::Display for SstableV2Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "NuDB SSTable v2 error: {self:?}")
    }
}

impl std::error::Error for SstableV2Error {}
