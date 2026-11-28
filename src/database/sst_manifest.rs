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
        let version: u16 = match self.recovery.as_ref() {
            None => 1,
            Some(meta) if meta.floor == 0 => 2,
            Some(_) => 3,
        };
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
        if !matches!(version, 1..=3) {
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
        if version >= 2 {
            let meta = RecoveryMeta::decode(
                &body[end..],
                manifest.tablet_id,
                manifest.epoch,
                manifest.durable_sequence(),
            )?;
            if (version == 2) != (meta.floor == 0) {
                return Err(invalid("manifest version and retention floor disagree"));
            }
            manifest.recovery = Some(meta);
        }
        Ok(manifest)
    }
}

/// Physical SST bytes (excluding manifests and WAL) for a shadow compaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactionStats {
    pub input_tables: usize,
    pub output_tables: usize,
    pub input_bytes: u64,
    pub output_bytes: u64,
    pub input_versions: usize,
    pub output_versions: usize,
}

#[derive(Debug)]
pub struct ManifestStore {
    dir: PathBuf,
    _lock: super::sidecar_lock::SidecarLock,
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

fn acquire(dir: &Path) -> io::Result<(PathBuf, super::sidecar_lock::SidecarLock)> {
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
    let lock = super::sidecar_lock::SidecarLock::acquire(lock, "manifest writer already held")?;
    if lock.metadata()?.len() != 0 {
        return Err(invalid("manifest directory retired or lock marker damaged"));
    }
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
        self.recovery_meta()?;
        self.restore_manifest_state(&self.manifest)
    }

    fn restore_manifest_state(
        &self,
        manifest: &Manifest,
    ) -> io::Result<super::tablet::MemoryTablet> {
        use super::tablet::{MemoryTablet, TabletSnapshotRow, TabletSnapshotState, VersionedValue};
        let meta = manifest
            .recovery
            .as_ref()
            .ok_or_else(|| invalid("manifest is not recovery sealed"))?;
        let mut rows = std::collections::BTreeMap::<Vec<u8>, Vec<VersionedValue>>::new();
        for entry in &manifest.entries {
            for row in self.open_verified_table(entry)?.into_snapshot_rows()? {
                rows.entry(row.key).or_default().extend(row.versions);
            }
        }
        let state = TabletSnapshotState {
            current_sequence: manifest.durable_sequence(),
            oldest_retained_snapshot: None,
            rows: rows
                .into_iter()
                .map(|(key, versions)| TabletSnapshotRow { key, versions })
                .collect(),
            receipts: meta.receipts.values().copied().collect(),
        };
        let mut tablet = MemoryTablet::restore_snapshot(meta.descriptor.clone(), state)
            .map_err(io::Error::other)?;
        if meta.floor != 0 {
            tablet
                .prune_history_before(meta.floor)
                .map_err(io::Error::other)?;
        }
        Ok(tablet)
    }

    pub fn recovered_receipt(&self, id: TransactionId) -> io::Result<Option<TransactionReceipt>> {
        Ok(self.recovery_meta()?.receipts.get(&id).copied())
    }
    /// Canonical sealed manifest identity; includes descriptor, receipts, floor,
    /// and every referenced immutable table digest. Not an authentication tag.
    pub(super) fn wal_binding(&self) -> io::Result<super::wal::SealBinding> {
        let meta = self.recovery_meta()?;
        let bytes = self.manifest.encode();
        Ok(super::wal::SealBinding {
            sequence: self.manifest.durable_sequence(),
            tablet_id: meta.descriptor.id(),
            epoch: meta.descriptor.ownership_epoch(),
            digest: bytes[bytes.len() - 32..]
                .try_into()
                .expect("manifest checksum"),
        })
    }
    /// Revalidate and synchronize the seal before allowing the full WAL to
    /// be discarded. The stable manifest lock remains held by this handle.
    /// Directory ancestors must already be durable and operator-controlled.
    pub(super) fn sync_for_wal_binding(&self) -> io::Result<super::wal::SealBinding> {
        let binding = self.wal_binding()?;
        for entry in &self.manifest.entries {
            self.open_verified_table(entry)?.sync_file()?;
        }
        let path = self.dir.join("MANIFEST");
        reject_alias(&path)?;
        let file = File::open(path)?;
        let mut bytes = Vec::new();
        (&file).take(MAX_BYTES as u64 + 1).read_to_end(&mut bytes)?;
        if bytes != self.manifest.encode() {
            return Err(invalid("sealed manifest changed before WAL binding"));
        }
        file.sync_all()?;
        File::open(&self.dir)?.sync_all()?;
        if let Some(parent) = self.dir.parent() {
            File::open(parent)?.sync_all()?;
        }
        Ok(binding)
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
    /// Build an independently verified compacted shadow of a complete sealed
    /// prefix at its already durable retention floor. No WAL binding changes,
    /// new retention floor, retirement authorization, or automatic cutover.
    /// Initial qualification admits at most 16 tables / 64 MiB encoded input
    /// (also bounds the temporary in-memory recovery oracle). Aggregate index
    /// keys and versions must fit one table's limits. Use a fresh destination
    /// after any failure, including a partially published shadow.
    pub fn prepare_compacted_shadow(&self, next: &mut Self) -> io::Result<CompactionStats> {
        const MAX_INPUT_TABLES: usize = 16;
        const MAX_INPUT_BYTES: u64 = 64 * 1024 * 1024;
        let meta = self.recovery_meta()?.clone();
        if next.requires_reopen
            || next.manifest.is_recovery_sealed()
            || !next.manifest.entries.is_empty()
            || self.manifest.tablet_id != next.manifest.tablet_id
            || self.manifest.epoch != next.manifest.epoch
            || self.dir.starts_with(&next.dir)
            || next.dir.starts_with(&self.dir)
        {
            return Err(invalid("compaction requires separate empty manifest"));
        }
        next.verify_dedicated_files()?;
        let input_tables = self.manifest.entries.len();
        let input_bytes = self.manifest.entries.iter().try_fold(0u64, |total, e| {
            total
                .checked_add(e.file_bytes)
                .ok_or_else(|| invalid("compaction byte overflow"))
        })?;
        if input_tables == 0 || input_tables > MAX_INPUT_TABLES || input_bytes > MAX_INPUT_BYTES {
            return Err(invalid("shadow compaction input budget exceeded"));
        }
        self.sync_for_wal_binding()?;
        let mut tables = Vec::with_capacity(input_tables);
        let mut input_versions = 0usize;
        let mut key_bytes = 0usize;
        for entry in &self.manifest.entries {
            let table = self.open_verified_table(entry)?;
            input_versions += table.indexed_versions();
            key_bytes += table.indexed_key_bytes();
            if input_versions > 262_144 || key_bytes > 16 * 1024 * 1024 {
                return Err(invalid("shadow compaction aggregate index budget exceeded"));
            }
            tables.push(table);
        }
        let path = table_path(&next.dir, 1);
        super::indexed_sst::compact_verified_tables(
            &path,
            tables,
            meta.floor,
            self.manifest.durable_sequence(),
        )?;
        let output = IndexedSst::open(&path)?;
        let output_versions = output.indexed_versions();
        let output_bytes = output.file_bytes();
        drop(output);
        next.append(TableEntry::inspect(&next.dir, 1)?)?;
        // Verify logical equivalence before publishing a recovery seal.
        let mut candidate = next.manifest.clone();
        candidate.recovery = Some(meta.clone());
        let expected = self.restore_memory_tablet()?.snapshot_state();
        let actual = next.restore_manifest_state(&candidate)?.snapshot_state();
        if actual != expected {
            return Err(invalid(
                "compacted shadow disagrees with independent recovery oracle",
            ));
        }
        next.seal(meta)?;
        next.sync_for_wal_binding()?;
        Ok(CompactionStats {
            input_tables,
            output_tables: 1,
            input_bytes,
            output_bytes,
            input_versions,
            output_versions,
        })
    }

    /// Start an independent generation with exact copies of the bound
    /// prefix. Never modify the previous generation or overwrite a file.
    pub(super) fn copy_prefix_into(&self, next: &mut Self) -> io::Result<()> {
        if self.requires_reopen
            || next.requires_reopen
            || !self.manifest.is_recovery_sealed()
            || next.manifest.is_recovery_sealed()
            || !next.manifest.entries.is_empty()
            || self.dir == next.dir
            || self.manifest.tablet_id != next.manifest.tablet_id
            || self.manifest.epoch != next.manifest.epoch
        {
            return Err(invalid("successor requires independent empty manifest"));
        }
        for entry in &self.manifest.entries {
            self.open_verified_table(entry)?
                .copy_to(&table_path(&next.dir, entry.generation))?;
            next.append(entry.clone())?;
        }
        Ok(())
    }

    /// Copy an exact sealed recovery generation into a fresh directory.
    pub(super) fn copy_seal_into(&self, dir: &Path) -> io::Result<super::wal::SealBinding> {
        let expected = self.sync_for_wal_binding()?;
        super::sealed_backup::new_directory(dir)?;
        let mut next = Self::create(dir, self.manifest.tablet_id, self.manifest.epoch)?;
        self.copy_prefix_into(&mut next)?;
        next.seal(self.recovery_meta()?.clone())?;
        let actual = next.sync_for_wal_binding()?;
        if actual != expected {
            return Err(invalid("copied seal identity mismatch"));
        }
        Ok(actual)
    }

    /// Only a strict older, exact physical prefix with matching historical
    /// receipts is eligible for quarantine; unrelated seals are not garbage.
    pub(super) fn verify_obsolete_prefix(&self, active: &Self) -> io::Result<()> {
        if self.manifest.durable_sequence() >= active.manifest.durable_sequence()
            || !active.manifest.entries.starts_with(&self.manifest.entries)
        {
            return Err(invalid("obsolete seal is not an exact older prefix"));
        }
        self.verify_retirement_scope(active)
    }

    /// Common checks for exact prefix and committed compaction lineage.
    pub(super) fn verify_retirement_scope(&self, active: &Self) -> io::Result<()> {
        let old = self.recovery_meta()?;
        let new = active.recovery_meta()?;
        if old.descriptor != new.descriptor
            || old.floor > new.floor
            || self.manifest.durable_sequence() > active.manifest.durable_sequence()
            || old
                .receipts
                .iter()
                .any(|(id, receipt)| new.receipts.get(id) != Some(receipt))
        {
            return Err(invalid("obsolete seal scope or receipts disagree"));
        }
        self.sync_for_wal_binding()?;
        Ok(())
    }

    /// Move the dedicated directory while retaining its stable lock. Never
    /// unlink files, recursively traverse, or move a directory with extra data.
    /// Caller coordinates the trusted parent namespace and all external users.
    pub(super) fn quarantine(self, destination: &Path) -> io::Result<()> {
        let parent = self
            .dir
            .parent()
            .ok_or_else(|| invalid("generation has no parent"))?;
        let name = destination
            .file_name()
            .ok_or_else(|| invalid("invalid quarantine name"))?;
        let dest_parent = destination
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."))
            .canonicalize()?;
        if dest_parent != parent {
            return Err(invalid("quarantine must be a fresh sibling directory"));
        }
        let target = dest_parent.join(name);
        match fs::symlink_metadata(&target) {
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "quarantine destination exists",
                ))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        self.verify_dedicated_files()?;
        File::open(parent)?.sync_all()?;
        #[cfg(test)]
        super::interruption::hit(
            super::interruption::StorageInterruptionPoint::QuarantineBeforeRename,
        )?;
        fs::rename(&self.dir, &target)?;
        #[cfg(test)]
        super::interruption::hit(
            super::interruption::StorageInterruptionPoint::QuarantineAfterRename,
        )?;
        File::open(parent)?.sync_all()?;
        Ok(())
    }

    /// Validate the exact allowlisted contents without moving or deleting data.
    pub(super) fn verify_dedicated_files(&self) -> io::Result<()> {
        let mut expected = std::collections::BTreeSet::new();
        expected.insert(std::ffi::OsString::from("MANIFEST"));
        expected.insert(std::ffi::OsString::from("MANIFEST.lock"));
        for entry in &self.manifest.entries {
            expected.insert(
                table_path(&self.dir, entry.generation)
                    .file_name()
                    .expect("table name")
                    .to_os_string(),
            );
        }
        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            if !expected.remove(&entry.file_name()) {
                return Err(invalid("unexpected file in obsolete generation"));
            }
            reject_alias(&entry.path())?;
        }
        if !expected.is_empty() {
            return Err(invalid("missing obsolete generation file"));
        }
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

#[cfg(test)]
mod lock_lifetime_tests {
    use super::*;

    #[test]
    fn manifest_logical_drop_releases_lock_with_duplicated_descriptor() {
        let dir = std::env::temp_dir().join(format!(
            "nudb-manifest-lock-lifetime-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&dir).unwrap();
        let first = ManifestStore::create(&dir, 7, 3).unwrap();
        let inherited = first._lock.duplicate_descriptor().unwrap();
        assert_eq!(
            ManifestStore::open(&dir, 7, 3).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        drop(first);
        let second = ManifestStore::open(&dir, 7, 3).unwrap();
        drop(inherited);
        assert_eq!(
            ManifestStore::open(&dir, 7, 3).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        drop(second);
        drop(ManifestStore::open(&dir, 7, 3).unwrap());
        fs::remove_dir_all(&dir).unwrap();
    }
}
