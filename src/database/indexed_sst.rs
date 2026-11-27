//! Read-only, value-on-demand index for the draft NUDBSST1 table format.
//!
//! Opening streams and verifies the entire table, but retains only keys,
//! version metadata, and value locations/digests. No mutable storage or WAL
//! integration is implied. Files must remain immutable while open; this
//! primitive is not a manifest, ownership protocol, or compaction engine.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom};
use std::path::Path;

const MAX_TABLE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_KEY_BYTES: usize = 4 * 1024 * 1024;
const MAX_VALUE_BYTES: usize = 64 * 1024 * 1024;
const MAX_INDEX_KEY_BYTES: usize = 16 * 1024 * 1024;
const MAX_INDEX_VERSIONS: usize = 262_144;

fn invalid(reason: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, reason)
}

#[derive(Debug, PartialEq, Eq)]
pub enum DiskLookup {
    Missing,
    Deleted { sequence: u64 },
    Value { sequence: u64, value: Vec<u8> },
}

#[derive(Debug)]
struct Location {
    offset: u64,
    length: usize,
    digest: blake3::Hash,
}

#[derive(Debug)]
struct Version {
    sequence: u64,
    value: Option<Location>,
}

#[derive(Debug)]
pub struct IndexedSst {
    file: File,
    generation: u64,
    min_sequence: u64,
    max_sequence: u64,
    version_count: usize,
    key_bytes: usize,
    rows: BTreeMap<Vec<u8>, Vec<Version>>,
}

struct Scanner {
    reader: BufReader<File>,
    position: u64,
    end: u64,
    digest: blake3::Hasher,
}

impl Scanner {
    fn remaining(&self) -> u64 {
        self.end - self.position
    }
    fn read(&mut self, buf: &mut [u8]) -> io::Result<()> {
        if buf.len() as u64 > self.remaining() {
            return Err(invalid("SST field crosses checksum boundary"));
        }
        self.reader.read_exact(buf)?;
        self.digest.update(buf);
        self.position += buf.len() as u64;
        Ok(())
    }
    fn fixed<const N: usize>(&mut self) -> io::Result<[u8; N]> {
        let mut bytes = [0; N];
        self.read(&mut bytes)?;
        Ok(bytes)
    }
    fn u32(&mut self) -> io::Result<u32> {
        Ok(u32::from_le_bytes(self.fixed()?))
    }
    fn u64(&mut self) -> io::Result<u64> {
        Ok(u64::from_le_bytes(self.fixed()?))
    }
    fn value(&mut self, length: usize, scratch: &mut [u8]) -> io::Result<Location> {
        if length > MAX_VALUE_BYTES || length as u64 > self.remaining() {
            return Err(invalid("invalid SST value length"));
        }
        let offset = self.position;
        let mut digest = blake3::Hasher::new();
        let mut left = length;
        while left != 0 {
            let n = left.min(scratch.len());
            self.read(&mut scratch[..n])?;
            digest.update(&scratch[..n]);
            left -= n;
        }
        Ok(Location {
            offset,
            length,
            digest: digest.finalize(),
        })
    }
}

impl IndexedSst {
    /// Open a regular immutable file. Corrupt/unsupported/oversized tables
    /// fail closed. A fixed scratch buffer is reused across all value scans.
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let file = File::open(path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() || !(82..=MAX_TABLE_BYTES).contains(&metadata.len()) {
            return Err(invalid("invalid SST file size or type"));
        }
        let mut scanner = Scanner {
            reader: BufReader::new(file),
            position: 0,
            end: metadata.len() - 32,
            digest: blake3::Hasher::new(),
        };
        if &scanner.fixed::<8>()? != b"NUDBSST1" || u16::from_le_bytes(scanner.fixed()?) != 1 {
            return Err(invalid("unsupported SST magic or version"));
        }
        let generation = scanner.u64()?;
        let min_sequence = scanner.u64()?;
        let max_sequence = scanner.u64()?;
        let key_count = scanner.u64()?;
        let declared_versions = scanner.u64()?;
        if generation == 0 || min_sequence == 0 || min_sequence > max_sequence {
            return Err(invalid("invalid SST generation or sequence interval"));
        }
        // Each row requires at least 8 bytes of lengths and one 9-byte
        // version. Reject impossible counts before allocation or iteration.
        if key_count > declared_versions
            || declared_versions > MAX_INDEX_VERSIONS as u64
            || key_count > scanner.remaining() / 17
            || declared_versions > scanner.remaining() / 9
        {
            return Err(invalid("invalid or excessive SST index counts"));
        }
        let mut rows = BTreeMap::<Vec<u8>, Vec<Version>>::new();
        let mut key_bytes = 0usize;
        let mut version_count = 0usize;
        let mut scratch = vec![0; 64 * 1024];
        for _ in 0..key_count {
            let key_len = scanner.u32()? as usize;
            key_bytes = key_bytes
                .checked_add(key_len)
                .ok_or_else(|| invalid("SST index overflow"))?;
            if key_len > MAX_KEY_BYTES
                || key_bytes > MAX_INDEX_KEY_BYTES
                || key_len as u64 > scanner.remaining()
            {
                return Err(invalid("invalid or excessive SST key length"));
            }
            let mut key = vec![0; key_len];
            scanner.read(&mut key)?;
            if rows
                .last_key_value()
                .is_some_and(|(previous, _)| previous >= &key)
            {
                return Err(invalid("SST keys must be strictly ordered"));
            }
            let count = scanner.u32()? as usize;
            version_count = version_count
                .checked_add(count)
                .ok_or_else(|| invalid("SST index overflow"))?;
            if count == 0
                || version_count as u64 > declared_versions
                || count as u64 > scanner.remaining() / 9
            {
                return Err(invalid("invalid SST row version count"));
            }
            let mut versions = Vec::with_capacity(count);
            let mut previous = 0;
            for _ in 0..count {
                let sequence = scanner.u64()?;
                if sequence <= previous || sequence < min_sequence || sequence > max_sequence {
                    return Err(invalid("invalid SST MVCC sequence"));
                }
                previous = sequence;
                let value = match scanner.fixed::<1>()?[0] {
                    0 => None,
                    1 => {
                        let length = scanner.u32()? as usize;
                        Some(scanner.value(length, &mut scratch)?)
                    }
                    _ => return Err(invalid("invalid SST value tag")),
                };
                versions.push(Version { sequence, value });
            }
            rows.insert(key, versions);
        }
        if version_count as u64 != declared_versions || scanner.remaining() != 0 {
            return Err(invalid("SST count or trailing data mismatch"));
        }
        let mut checksum = [0; 32];
        scanner.reader.read_exact(&mut checksum)?;
        if scanner.digest.finalize().as_bytes() != &checksum {
            return Err(invalid("SST checksum mismatch"));
        }
        let mut extra = [0; 1];
        if scanner.reader.read(&mut extra)? != 0 {
            return Err(invalid("SST changed length during open"));
        }
        Ok(Self {
            file: scanner.reader.into_inner(),
            generation,
            min_sequence,
            max_sequence,
            version_count,
            key_bytes,
            rows,
        })
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }
    pub fn sequence_interval(&self) -> (u64, u64) {
        (self.min_sequence, self.max_sequence)
    }
    pub fn indexed_versions(&self) -> usize {
        self.version_count
    }
    pub fn indexed_key_bytes(&self) -> usize {
        self.key_bytes
    }

    /// Read a table-local visible version. Snapshots beyond this table's max
    /// may select its latest version; global snapshot validity is the caller's
    /// responsibility. Distinguishes deletion from no visible version.
    /// I/O errors and mismatching value digests never masquerade as absence.
    pub fn read_at(&mut self, key: &[u8], snapshot: u64) -> io::Result<DiskLookup> {
        let Some(versions) = self.rows.get(key) else {
            return Ok(DiskLookup::Missing);
        };
        let end = versions.partition_point(|v| v.sequence <= snapshot);
        let Some(version) = end.checked_sub(1).and_then(|i| versions.get(i)) else {
            return Ok(DiskLookup::Missing);
        };
        let Some(location) = &version.value else {
            return Ok(DiskLookup::Deleted {
                sequence: version.sequence,
            });
        };
        self.file.seek(SeekFrom::Start(location.offset))?;
        let mut value = vec![0; location.length];
        self.file.read_exact(&mut value)?;
        if blake3::hash(&value) != location.digest {
            return Err(invalid("SST value changed since index validation"));
        }
        Ok(DiskLookup::Value {
            sequence: version.sequence,
            value,
        })
    }
}

/// Exact counts for one immutable committed sequence interval. Empty intervals
/// of mutations still require a nonempty sequence interval.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SstHeader {
    pub generation: u64,
    pub min_sequence: u64,
    pub max_sequence: u64,
    pub key_count: u64,
    pub version_count: u64,
}

pub type SstVersion<'a> = (u64, Option<&'a [u8]>);
pub type SstRow<'a> = (&'a [u8], &'a [SstVersion<'a>]);

/// Stream one sorted immutable table without buffering its encoded contents.
/// Rows and versions must be strictly ordered, and header counts must match.
/// The destination must not exist. On Unix, publication uses a synchronized
/// same-directory staging file, an exclusive hard link, and directory sync.
/// Requires a trusted directory and a local filesystem supporting these
/// operations. An error after publication may leave a complete table; callers
/// must reconcile before retrying. This is not manifest/WAL publication.
pub fn write_immutable_table<'a>(
    path: impl AsRef<Path>,
    header: SstHeader,
    rows: impl IntoIterator<Item = SstRow<'a>>,
) -> io::Result<()> {
    #[cfg(unix)]
    {
        streaming_writer::write(path.as_ref(), header, rows)
    }
    #[cfg(not(unix))]
    {
        let _ = (path, header, rows);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "durable SST publication is supported on Unix only",
        ))
    }
}

#[cfg(unix)]
mod streaming_writer {
    use super::*;
    use std::fs::{self, OpenOptions};
    use std::io::{BufWriter, Write};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct Staged(PathBuf);
    impl Drop for Staged {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }

    struct Encoder {
        writer: BufWriter<File>,
        digest: blake3::Hasher,
        bytes: u64,
    }
    impl Encoder {
        fn emit(&mut self, data: &[u8]) -> io::Result<()> {
            let bytes = self
                .bytes
                .checked_add(data.len() as u64)
                .ok_or_else(|| invalid("SST size overflow"))?;
            if bytes > MAX_TABLE_BYTES - 32 {
                return Err(invalid("SST exceeds table size limit"));
            }
            self.writer.write_all(data)?;
            self.digest.update(data);
            self.bytes = bytes;
            Ok(())
        }
    }

    fn stage(parent: &Path, destination: &Path) -> io::Result<(Staged, File)> {
        for _ in 0..64 {
            let path = parent.join(format!(
                ".nudb-sst-{}-{}.tmp",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            if path == destination {
                continue;
            }
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => return Ok((Staged(path), file)),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not allocate SST staging file",
        ))
    }

    pub(super) fn write<'a>(
        path: &Path,
        header: SstHeader,
        rows: impl IntoIterator<Item = SstRow<'a>>,
    ) -> io::Result<()> {
        if header.generation == 0
            || header.min_sequence == 0
            || header.min_sequence > header.max_sequence
            || header.key_count > header.version_count
            || header.version_count > MAX_INDEX_VERSIONS as u64
        {
            return Err(invalid("invalid SST header"));
        }
        let filename = path
            .file_name()
            .ok_or_else(|| invalid("missing SST filename"))?;
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."))
            .canonicalize()?;
        let destination = parent.join(filename);
        let (staged, file) = stage(&parent, &destination)?;
        let mut encoder = Encoder {
            writer: BufWriter::new(file),
            digest: blake3::Hasher::new(),
            bytes: 0,
        };
        encoder.emit(b"NUDBSST1")?;
        encoder.emit(&1u16.to_le_bytes())?;
        for n in [
            header.generation,
            header.min_sequence,
            header.max_sequence,
            header.key_count,
            header.version_count,
        ] {
            encoder.emit(&n.to_le_bytes())?;
        }
        let mut previous_key: Option<&[u8]> = None;
        let mut keys = 0u64;
        let mut versions = 0u64;
        let mut key_bytes = 0usize;
        for (key, history) in rows {
            if previous_key.is_some_and(|prev| prev >= key) {
                return Err(invalid("SST keys must be strictly ordered"));
            }
            previous_key = Some(key);
            key_bytes = key_bytes
                .checked_add(key.len())
                .ok_or_else(|| invalid("SST key bytes overflow"))?;
            keys = keys
                .checked_add(1)
                .ok_or_else(|| invalid("SST key count overflow"))?;
            versions = versions
                .checked_add(history.len() as u64)
                .ok_or_else(|| invalid("SST version count overflow"))?;
            if key.len() > MAX_KEY_BYTES
                || key_bytes > MAX_INDEX_KEY_BYTES
                || history.is_empty()
                || keys > header.key_count
                || versions > header.version_count
            {
                return Err(invalid("SST row exceeds declared counts or index limits"));
            }
            encoder.emit(&(key.len() as u32).to_le_bytes())?;
            encoder.emit(key)?;
            encoder.emit(&(history.len() as u32).to_le_bytes())?;
            let mut previous = 0;
            for &(sequence, value) in history {
                if sequence <= previous
                    || sequence < header.min_sequence
                    || sequence > header.max_sequence
                {
                    return Err(invalid("invalid SST MVCC sequence"));
                }
                previous = sequence;
                encoder.emit(&sequence.to_le_bytes())?;
                match value {
                    None => encoder.emit(&[0])?,
                    Some(value) => {
                        if value.len() > MAX_VALUE_BYTES {
                            return Err(invalid("SST value too large"));
                        }
                        encoder.emit(&[1])?;
                        encoder.emit(&(value.len() as u32).to_le_bytes())?;
                        encoder.emit(value)?;
                    }
                }
            }
        }
        if keys != header.key_count || versions != header.version_count {
            return Err(invalid("SST count mismatch"));
        }
        encoder
            .writer
            .write_all(encoder.digest.finalize().as_bytes())?;
        encoder.writer.flush()?;
        encoder.writer.get_ref().sync_all()?;
        drop(encoder);

        // Unlike rename on Unix, hard_link refuses to replace an existing
        // immutable generation. Sync directory entries before acknowledgement.
        fs::hard_link(&staged.0, &destination)?;
        fs::remove_file(&staged.0)?;
        File::open(&parent)?.sync_all()?;
        Ok(())
    }
}
