//! Append-only manifest for immutable SST sequence intervals.
//!
//! One cooperative writer owns the directory; the manifest, not directory
//! discovery, selects authoritative tables. This is a Unix-only primitive
//! in a trusted local directory. No WAL reclamation, snapshot-floor,
//! compaction, or live tablet integration is implied. A v2 sealed manifest
//! additionally records a read-only recovery snapshot.

use super::indexed_sst::{DiskLookup, IndexedSst};
use super::sst_recovery_meta::{RecoveryMeta, MAX_META_BYTES};
use super::tablet::{TabletDescriptor, TransactionId, TransactionReceipt};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const MAGIC: &[u8; 8] = b"NUDBMAN1";
const MAX_ENTRIES: usize = 1024;
const HEADER_BYTES: usize = 34;
const ENTRY_BYTES: usize = 64;
const MAX_BYTES: usize = HEADER_BYTES + ENTRY_BYTES * MAX_ENTRIES + MAX_META_BYTES + 32;
static NEXT: AtomicU64 = AtomicU64::new(0);

fn invalid(reason: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, reason)
}

fn reject_alias(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if !meta.is_file() || meta.nlink() != 1 => {
            Err(invalid("manifest path is not a unique regular file"))
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn table_path(dir: &Path, generation: u64) -> PathBuf {
    dir.join(format!("nudb-sst-{generation:020}.sst"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableEntry {
    generation: u64,
    min_sequence: u64,
    max_sequence: u64,
    file_bytes: u64,
    checksum: [u8; 32],
}

impl TableEntry {
    pub fn inspect(dir: impl AsRef<Path>, generation: u64) -> io::Result<Self> {
        Self::inspect_inner(dir.as_ref(), generation, false)
    }
    fn inspect_inner(dir: &Path, generation: u64, sync: bool) -> io::Result<Self> {
        let path = table_path(dir, generation);
        reject_alias(&path)?;
        let table = IndexedSst::open(path)?;
        if table.generation() != generation {
            return Err(invalid("SST generation does not match its filename"));
        }
        if sync {
            table.sync_file()?;
        }
        let (min_sequence, max_sequence) = table.sequence_interval();
        Ok(Self {
            generation,
            min_sequence,
            max_sequence,
            file_bytes: table.file_bytes(),
            checksum: table.checksum(),
        })
    }
    pub fn generation(&self) -> u64 {
        self.generation
    }
    pub fn sequence_interval(&self) -> (u64, u64) {
        (self.min_sequence, self.max_sequence)
    }
    pub fn checksum(&self) -> [u8; 32] {
        self.checksum
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    tablet_id: u64,
    epoch: u64,
    entries: Vec<TableEntry>,
    recovery: Option<RecoveryMeta>,
}
impl Manifest {
    pub fn empty(tablet_id: u64, epoch: u64) -> io::Result<Self> {
        if tablet_id == 0 || epoch == 0 {
            return Err(invalid("invalid manifest tablet identity"));
        }
        Ok(Self {
            tablet_id,
            epoch,
            entries: Vec::new(),
            recovery: None,
        })
    }
    pub fn tablet_id(&self) -> u64 {
        self.tablet_id
    }
    pub fn epoch(&self) -> u64 {
        self.epoch
    }
    pub fn entries(&self) -> &[TableEntry] {
        &self.entries
    }
    pub fn durable_sequence(&self) -> u64 {
        self.entries.last().map_or(0, |e| e.max_sequence)
    }
    pub fn is_recovery_sealed(&self) -> bool {
        self.recovery.is_some()
    }
    pub fn recovery_descriptor(&self) -> Option<&TabletDescriptor> {
        self.recovery.as_ref().map(|meta| &meta.descriptor)
    }
    pub fn recovery_floor(&self) -> Option<u64> {
        self.recovery.as_ref().map(|meta| meta.floor)
    }
    fn push(&mut self, entry: TableEntry) -> io::Result<()> {
        if self.recovery.is_some() {
            return Err(invalid("sealed manifest cannot append"));
        }
        if self.entries.len() >= MAX_ENTRIES {
            return Err(invalid("manifest entry limit exceeded"));
        }
        let min = self
            .durable_sequence()
            .checked_add(1)
            .ok_or_else(|| invalid("manifest sequence overflow"))?;
        if entry.generation != self.entries.len() as u64 + 1
            || entry.min_sequence != min
            || entry.max_sequence < min
            || !(82..=256 * 1024 * 1024).contains(&entry.file_bytes)
        {
            return Err(invalid(
                "manifest sequence interval or generation is not contiguous",
            ));
        }
        self.entries.push(entry);
        Ok(())
    }
    fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(HEADER_BYTES + ENTRY_BYTES * self.entries.len() + 32);
        bytes.extend_from_slice(MAGIC);
        let version: u16 = if self.recovery.is_some() { 2 } else { 1 };
        bytes.extend_from_slice(&version.to_le_bytes());
        for n in [self.tablet_id, self.epoch, self.entries.len() as u64] {
            bytes.extend_from_slice(&n.to_le_bytes());
        }
        for e in &self.entries {
            for n in [e.generation, e.min_sequence, e.max_sequence, e.file_bytes] {
                bytes.extend_from_slice(&n.to_le_bytes());
            }
            bytes.extend_from_slice(&e.checksum);
        }
        if let Some(meta) = &self.recovery {
            meta.encode(&mut bytes);
        }
        let hash = blake3::hash(&bytes);
        bytes.extend_from_slice(hash.as_bytes());
        bytes
    }
    fn decode(bytes: &[u8]) -> io::Result<Self> {
        if bytes.len() < HEADER_BYTES + 32 || bytes.len() > MAX_BYTES || &bytes[..8] != MAGIC {
            return Err(invalid("unsupported or malformed manifest"));
        }
        let version = u16::from_le_bytes(bytes[8..10].try_into().expect("fixed version"));
        if !matches!(version, 1 | 2) {
            return Err(invalid("unsupported manifest version"));
        }
        let (body, hash) = bytes.split_at(bytes.len() - 32);
        if blake3::hash(body).as_bytes() != hash {
            return Err(invalid("manifest checksum mismatch"));
        }
        let read = |b: &[u8], offset: usize| {
            u64::from_le_bytes(
                b[offset..offset + 8]
                    .try_into()
                    .expect("fixed manifest field"),
            )
        };
        let count = read(body, 26);
        if count > MAX_ENTRIES as u64 {
            return Err(invalid("manifest count limit"));
        }
        let end = HEADER_BYTES + ENTRY_BYTES * count as usize;
        if body.len() < end || (version == 1 && body.len() != end) {
            return Err(invalid("manifest count or length mismatch"));
        }
        let mut manifest = Self::empty(read(body, 10), read(body, 18))?;
        for record in body[HEADER_BYTES..end].chunks_exact(ENTRY_BYTES) {
            manifest.push(TableEntry {
                generation: read(record, 0),
                min_sequence: read(record, 8),
                max_sequence: read(record, 16),
                file_bytes: read(record, 24),
                checksum: record[32..64].try_into().expect("fixed checksum"),
            })?;
        }
        if version == 2 {
            manifest.recovery = Some(RecoveryMeta::decode(
                &body[end..],
                manifest.tablet_id,
                manifest.epoch,
                manifest.durable_sequence(),
            )?);
        }
        Ok(manifest)
    }
}

#[derive(Debug)]
pub struct ManifestStore {
    dir: PathBuf,
    _lock: File,
    manifest: Manifest,
    requires_reopen: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Write,
    FileSync,
    Rename,
    DirectorySync,
}

struct Staged(PathBuf);
impl Drop for Staged {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn acquire(dir: &Path) -> io::Result<(PathBuf, File)> {
    let dir = dir.canonicalize()?;
    let lock_path = dir.join("MANIFEST.lock");
    reject_alias(&lock_path)?;
    // Stable sidecar: never remove while any owner may exist.
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)?;
    lock.try_lock().map_err(|error| match error {
        std::fs::TryLockError::WouldBlock => {
            io::Error::new(io::ErrorKind::WouldBlock, "manifest writer already held")
        }
        std::fs::TryLockError::Error(error) => error,
    })?;
    Ok((dir, lock))
}

fn publish(
    dir: &Path,
    manifest: &Manifest,
    initial: bool,
    mut hook: impl FnMut(Phase) -> io::Result<()>,
) -> io::Result<()> {
    let destination = dir.join("MANIFEST");
    reject_alias(&destination)?;
    if initial {
        match fs::symlink_metadata(&destination) {
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "manifest already exists",
                ))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    let mut stage = None;
    for _ in 0..64 {
        let path = dir.join(format!(
            ".MANIFEST-{}-{}.tmp",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        match OpenOptions::new().create_new(true).write(true).open(&path) {
            Ok(file) => {
                stage = Some((Staged(path), file));
                break;
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    let (staged, mut file) = stage.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::AlreadyExists,
            "manifest staging collision limit",
        )
    })?;
    file.write_all(&manifest.encode())?;
    hook(Phase::Write)?;
    file.sync_all()?;
    hook(Phase::FileSync)?;
    drop(file);
    // The stable cooperative lock serializes initialization and updates.
    // Rename avoids a crash window where the final name and staging name
    // share an inode and alias rejection would prevent recovery.
    fs::rename(&staged.0, &destination)?;
    hook(Phase::Rename)?;
    File::open(dir)?.sync_all()?;
    hook(Phase::DirectorySync)?;
    Ok(())
}

impl ManifestStore {
    /// Explicit initialization only. Existing or corrupt manifests are never replaced.
    pub fn create(dir: impl AsRef<Path>, tablet_id: u64, epoch: u64) -> io::Result<Self> {
        let manifest = Manifest::empty(tablet_id, epoch)?;
        let (dir, lock) = acquire(dir.as_ref())?;
        for entry in fs::read_dir(&dir)? {
            let name = entry?.file_name();
            if name.as_encoded_bytes().starts_with(b"nudb-sst-") {
                return Err(invalid("refusing to initialize over existing SSTables"));
            }
        }
        publish(&dir, &manifest, true, |_| Ok(()))?;
        Ok(Self {
            dir,
            _lock: lock,
            manifest,
            requires_reopen: false,
        })
    }
    /// Recover the authoritative manifest and verify each referenced table,
    /// one at a time. Missing manifest is an error, not an empty database.
    pub fn open(dir: impl AsRef<Path>, tablet_id: u64, epoch: u64) -> io::Result<Self> {
        let (dir, lock) = acquire(dir.as_ref())?;
        let path = dir.join("MANIFEST");
        reject_alias(&path)?;
        let mut bytes = Vec::new();
        File::open(path)?
            .take(MAX_BYTES as u64 + 1)
            .read_to_end(&mut bytes)?;
        let manifest = Manifest::decode(&bytes)?;
        if manifest.tablet_id != tablet_id || manifest.epoch != epoch {
            return Err(invalid("manifest tablet identity or epoch mismatch"));
        }
        let store = Self {
            dir,
            _lock: lock,
            manifest,
            requires_reopen: false,
        };
        for entry in &store.manifest.entries {
            store.open_verified_table(entry)?;
        }
        Ok(store)
    }
    /// Last acknowledged manifest, not authoritative after an ambiguous error.
    pub fn current(&self) -> &Manifest {
        &self.manifest
    }
    pub fn requires_reopen(&self) -> bool {
        self.requires_reopen
    }
    pub(crate) fn directory(&self) -> &Path {
        &self.dir
    }
    pub(crate) fn open_verified_table(&self, entry: &TableEntry) -> io::Result<IndexedSst> {
        let path = table_path(&self.dir, entry.generation);
        reject_alias(&path)?;
        let table = IndexedSst::open(path)?;
        if table.generation() != entry.generation
            || table.sequence_interval() != entry.sequence_interval()
            || table.file_bytes() != entry.file_bytes
            || table.checksum() != entry.checksum
        {
            return Err(invalid("manifest table identity mismatch"));
        }
        if let Some(descriptor) = self.manifest.recovery_descriptor() {
            if !table.keys_within(descriptor.range()) {
                return Err(invalid("SST outside sealed key range"));
            }
        }
        Ok(table)
    }
    /// Reads only a sealed, immutable recovery snapshot, never the live WAL tail.
    pub fn recovered_read_at(&self, key: &[u8], snapshot: u64) -> io::Result<Option<Vec<u8>>> {
        let meta = self.recovery_meta()?;
        if snapshot < meta.floor
            || snapshot > self.manifest.durable_sequence()
            || !meta.descriptor.range().contains(key)
        {
            return Err(invalid("snapshot or key outside sealed recovery state"));
        }
        for entry in self.manifest.entries.iter().rev() {
            if entry.min_sequence > snapshot {
                continue;
            }
            match self.open_verified_table(entry)?.read_at(key, snapshot)? {
                DiskLookup::Missing => {}
                DiskLookup::Deleted { .. } => return Ok(None),
                DiskLookup::Value { value, .. } => return Ok(Some(value)),
            }
        }
        Ok(None)
    }
    /// Rebuild the existing in-memory MVCC state from sealed disk tables.
    /// The caller must verify WAL correspondence before using it for writes.
    pub(super) fn restore_memory_tablet(&self) -> io::Result<super::tablet::MemoryTablet> {
        use super::tablet::{MemoryTablet, TabletSnapshotRow, TabletSnapshotState, VersionedValue};
        let meta = self.recovery_meta()?;
        let mut rows = std::collections::BTreeMap::<Vec<u8>, Vec<VersionedValue>>::new();
        for entry in &self.manifest.entries {
            for row in self.open_verified_table(entry)?.into_snapshot_rows()? {
                rows.entry(row.key).or_default().extend(row.versions);
            }
        }
        let state = TabletSnapshotState {
            current_sequence: self.manifest.durable_sequence(),
            oldest_retained_snapshot: (meta.floor != 0).then_some(meta.floor),
            rows: rows
                .into_iter()
                .map(|(key, versions)| TabletSnapshotRow { key, versions })
                .collect(),
            receipts: meta.receipts.values().copied().collect(),
        };
        MemoryTablet::restore_snapshot(meta.descriptor.clone(), state)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))
    }

    pub fn recovered_receipt(&self, id: TransactionId) -> io::Result<Option<TransactionReceipt>> {
        Ok(self.recovery_meta()?.receipts.get(&id).copied())
    }
    fn recovery_meta(&self) -> io::Result<&RecoveryMeta> {
        if self.requires_reopen {
            return Err(io::Error::other("manifest publication requires reopen"));
        }
        self.manifest
            .recovery
            .as_ref()
            .ok_or_else(|| invalid("manifest is not recovery sealed"))
    }
    pub(super) fn seal(&mut self, meta: RecoveryMeta) -> io::Result<()> {
        self.seal_with_hook(meta, |_| Ok(()))
    }
    fn seal_with_hook(
        &mut self,
        meta: RecoveryMeta,
        hook: impl FnMut(Phase) -> io::Result<()>,
    ) -> io::Result<()> {
        if self.requires_reopen {
            return Err(io::Error::other("manifest publication requires reopen"));
        }
        if meta.descriptor.id().get() != self.manifest.tablet_id
            || meta.descriptor.ownership_epoch() != self.manifest.epoch
        {
            return Err(invalid("recovery metadata identity mismatch"));
        }
        meta.validate(self.manifest.durable_sequence())?;
        if let Some(existing) = &self.manifest.recovery {
            return if existing == &meta {
                Ok(())
            } else {
                Err(invalid("sealed metadata is immutable"))
            };
        }
        let mut next = self.manifest.clone();
        next.recovery = Some(meta);
        if let Err(error) = publish(&self.dir, &next, false, hook) {
            self.requires_reopen = true;
            return Err(error);
        }
        self.manifest = next;
        Ok(())
    }
    pub fn append(&mut self, entry: TableEntry) -> io::Result<()> {
        self.append_with_hook(entry, |_| Ok(()))
    }
    fn append_with_hook(
        &mut self,
        entry: TableEntry,
        hook: impl FnMut(Phase) -> io::Result<()>,
    ) -> io::Result<()> {
        if self.requires_reopen {
            return Err(io::Error::other("manifest publication requires reopen"));
        }
        let mut next = self.manifest.clone();
        next.push(entry.clone())?;
        if TableEntry::inspect_inner(&self.dir, entry.generation, true)? != entry {
            return Err(invalid("manifest table identity mismatch"));
        }
        // Persist table directory entry before publishing a manifest reference.
        File::open(&self.dir)?.sync_all()?;
        if let Err(error) = publish(&self.dir, &next, false, hook) {
            self.requires_reopen = true;
            return Err(error);
        }
        self.manifest = next;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::indexed_sst::{write_immutable_table, SstHeader};
    #[test]
    fn publication_interruption_matrix_reopens_old_or_new_manifest() {
        for point in [
            Phase::Write,
            Phase::FileSync,
            Phase::Rename,
            Phase::DirectorySync,
        ] {
            let dir = std::env::temp_dir().join(format!(
                "nudb-manifest-fault-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&dir).unwrap();
            let mut store = ManifestStore::create(&dir, 7, 3).unwrap();
            write_immutable_table(
                table_path(&dir, 1),
                SstHeader {
                    generation: 1,
                    min_sequence: 1,
                    max_sequence: 2,
                    key_count: 0,
                    version_count: 0,
                },
                std::iter::empty(),
            )
            .unwrap();
            let entry = TableEntry::inspect(&dir, 1).unwrap();
            let result = store.append_with_hook(entry.clone(), |phase| {
                if phase == point {
                    Err(io::Error::other("injected publication interruption"))
                } else {
                    Ok(())
                }
            });
            assert!(result.is_err());
            assert!(store.requires_reopen());
            assert_eq!(store.current().durable_sequence(), 0);
            assert!(store.append(entry.clone()).is_err());
            drop(store);
            let mut reopened = ManifestStore::open(&dir, 7, 3).unwrap();
            if matches!(point, Phase::Write | Phase::FileSync) {
                assert_eq!(reopened.current().durable_sequence(), 0);
                reopened.append(entry).unwrap();
            }
            assert_eq!(reopened.current().durable_sequence(), 2);
            drop(reopened);
            fs::remove_dir_all(dir).unwrap();
        }
    }
}

#[cfg(test)]
mod seal_tests {
    use super::*;
    use crate::database::tablet::{KeyRange, TabletId};
    #[test]
    fn seal_interruption_preserves_unsealed_or_complete_metadata() {
        for point in [
            Phase::Write,
            Phase::FileSync,
            Phase::Rename,
            Phase::DirectorySync,
        ] {
            let dir = std::env::temp_dir().join(format!(
                "nudb-seal-fault-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&dir).unwrap();
            let mut store = ManifestStore::create(&dir, 7, 3).unwrap();
            let descriptor = TabletDescriptor::new(
                TabletId::new(7).unwrap(),
                KeyRange::new(b"a".to_vec(), None).unwrap(),
                3,
            )
            .unwrap();
            let meta = RecoveryMeta::new(descriptor.clone(), 0, 0, std::iter::empty()).unwrap();
            assert!(store
                .seal_with_hook(meta.clone(), |phase| if phase == point {
                    Err(io::Error::other("injected seal interruption"))
                } else {
                    Ok(())
                })
                .is_err());
            assert!(store.requires_reopen());
            assert!(store.recovered_read_at(b"a", 0).is_err());
            assert!(store.seal(meta.clone()).is_err());
            drop(store);
            let mut reopened = ManifestStore::open(&dir, 7, 3).unwrap();
            if matches!(point, Phase::Write | Phase::FileSync) {
                assert!(!reopened.current().is_recovery_sealed());
                reopened.seal(meta).unwrap();
            }
            assert_eq!(reopened.current().recovery_descriptor(), Some(&descriptor));
            assert_eq!(reopened.current().recovery_floor(), Some(0));
            assert_eq!(reopened.recovered_read_at(b"a", 0).unwrap(), None);
            drop(reopened);
            fs::remove_dir_all(dir).unwrap();
        }
    }
}
