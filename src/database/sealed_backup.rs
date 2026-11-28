//! Consistent local backup of a sealed manifest and exact acknowledged WAL tail.
//! Checksums detect accidental damage, not malicious rewriting. Trusted Unix
//! directories only. Incomplete destinations must never be reused.
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::Path;

const MAGIC: &[u8; 8] = b"NUDBBAK1";
const MARKER_BYTES: usize = 128;

fn invalid(reason: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, reason)
}

pub(super) fn new_directory(path: &Path) -> io::Result<()> {
    fs::DirBuilder::new().mode(0o700).create(path)
}

pub(super) fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

fn parent(path: &Path) -> &Path {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn unique_file(path: &Path) -> io::Result<File> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.nlink() != 1 {
        return Err(invalid("backup input must be a unique regular file"));
    }
    File::open(path)
}

fn exclusive_file(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

/// Copy exactly the expected number of bytes with bounded working memory.
/// Never opens the source as a writable WAL, so a torn backup is not repaired.
pub(super) fn copy_wal(source: &Path, dest: &Path, expected_len: u64) -> io::Result<[u8; 32]> {
    let mut input = unique_file(source)?;
    if input.metadata()?.len() != expected_len || expected_len < 96 {
        return Err(invalid("backup WAL length mismatch"));
    }
    let mut output = exclusive_file(dest)?;
    let mut hash = blake3::Hasher::new();
    let mut buf = [0u8; 64 * 1024];
    let mut remaining = expected_len;
    while remaining != 0 {
        let n = remaining.min(buf.len() as u64) as usize;
        input.read_exact(&mut buf[..n])?;
        hash.update(&buf[..n]);
        output.write_all(&buf[..n])?;
        remaining -= n as u64;
    }
    if input.read(&mut buf[..1])? != 0 {
        return Err(invalid("backup WAL grew during copy"));
    }
    output.sync_all()?;
    Ok(*hash.finalize().as_bytes())
}

/// Verify a complete WAL without opening it for recovery or changing bytes.
pub(super) fn wal_digest(path: &Path, expected_len: u64) -> io::Result<[u8; 32]> {
    let mut input = unique_file(path)?;
    if input.metadata()?.len() != expected_len || expected_len < 96 {
        return Err(invalid("backup WAL length mismatch"));
    }
    let mut hash = blake3::Hasher::new();
    let mut buf = [0u8; 64 * 1024];
    let mut remaining = expected_len;
    while remaining != 0 {
        let n = remaining.min(buf.len() as u64) as usize;
        input.read_exact(&mut buf[..n])?;
        hash.update(&buf[..n]);
        remaining -= n as u64;
    }
    if input.read(&mut buf[..1])? != 0 {
        return Err(invalid("WAL grew during verification"));
    }
    Ok(*hash.finalize().as_bytes())
}

/// Export/hash the canonical portable WAL4 representation. A managed WAL5
/// is validated against its exact live header, then only the header is
/// normalized. Frame bytes are preserved; the source is never changed.
pub(super) fn portable_wal(
    wal: &super::wal::FileWal,
    destination: Option<&Path>,
) -> io::Result<(u64, [u8; 32])> {
    let (source, portable, len) = wal.portable_headers().map_err(io::Error::other)?;
    let mut input = unique_file(wal.path())?;
    if input.metadata()?.len() != len {
        return Err(invalid("live WAL length mismatch"));
    }
    let mut actual = vec![0; source.len()];
    input.read_exact(&mut actual)?;
    if actual != source {
        return Err(invalid("live WAL header mismatch"));
    }
    let mut output = destination.map(exclusive_file).transpose()?;
    let mut hash = blake3::Hasher::new();
    hash.update(&portable);
    if let Some(out) = output.as_mut() {
        out.write_all(&portable)?;
    }
    let tail = len
        .checked_sub(source.len() as u64)
        .ok_or_else(|| invalid("live WAL length mismatch"))?;
    let mut remaining = tail;
    let mut buf = [0; 64 * 1024];
    while remaining != 0 {
        let n = remaining.min(buf.len() as u64) as usize;
        input.read_exact(&mut buf[..n])?;
        hash.update(&buf[..n]);
        if let Some(out) = output.as_mut() {
            out.write_all(&buf[..n])?;
        }
        remaining -= n as u64;
    }
    if input.read(&mut buf[..1])? != 0 {
        return Err(invalid("live WAL grew"));
    }
    if let Some(out) = output {
        out.sync_all()?;
    }
    Ok((portable.len() as u64 + tail, *hash.finalize().as_bytes()))
}

pub(super) struct Completion {
    pub sequence: u64,
    pub floor: u64,
    pub wal_len: u64,
    pub wal_digest: [u8; 32],
    pub seal_digest: [u8; 32],
}

impl Completion {
    pub(super) fn publish(&self, dir: &Path) -> io::Result<()> {
        let mut bytes = Vec::with_capacity(MARKER_BYTES);
        bytes.extend_from_slice(MAGIC);
        for value in [self.sequence, self.floor, self.wal_len] {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        bytes.extend_from_slice(&self.wal_digest);
        bytes.extend_from_slice(&self.seal_digest);
        let checksum = blake3::hash(&bytes);
        bytes.extend_from_slice(checksum.as_bytes());
        // The bundle directory is freshly created. A missing/partial marker
        // never qualifies for restore; both directory syncs precede success.
        sync_directory(dir)?;
        let mut file = exclusive_file(&dir.join("COMPLETE"))?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        sync_directory(dir)?;
        sync_directory(parent(dir))
    }

    pub(super) fn read(dir: &Path) -> io::Result<Self> {
        let mut bytes = [0; MARKER_BYTES];
        let mut file = unique_file(&dir.join("COMPLETE"))?;
        if file.metadata()?.len() != MARKER_BYTES as u64 {
            return Err(invalid("invalid backup completion length"));
        }
        file.read_exact(&mut bytes)?;
        if &bytes[..8] != MAGIC || blake3::hash(&bytes[..96]).as_bytes() != &bytes[96..] {
            return Err(invalid("invalid backup completion checksum"));
        }
        let read = |offset: usize| {
            u64::from_le_bytes(bytes[offset..offset + 8].try_into().expect("fixed field"))
        };
        let result = Self {
            sequence: read(8),
            floor: read(16),
            wal_len: read(24),
            wal_digest: bytes[32..64].try_into().expect("fixed digest"),
            seal_digest: bytes[64..96].try_into().expect("fixed digest"),
        };
        if result.sequence == 0 || result.floor > result.sequence || result.wal_len < 96 {
            return Err(invalid("invalid backup completion metadata"));
        }
        Ok(result)
    }
}
