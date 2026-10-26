//! Single-owner, single-tablet *local* backup capture.
//!
//! The live WalBackedTablet holds the exclusive sidecar writer lock, and its
//! shared Rust borrow prevents in-process mutations during the entire copy.
//! An incomplete directory with no COMPLETE.json is never an authoritative
//! backup. No remote storage, multi-node fencing, or live file-copy shortcut.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::store::{CheckpointWalReport, WalBackedError};
use super::tablet::TabletDescriptor;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupReceipt {
    pub destination: PathBuf,
    pub sequence: u64,
    pub wal_bytes: u64,
    pub checkpoint_bytes: Option<u64>,
    pub wal_blake3: String,
    pub checkpoint_blake3: Option<String>,
}

#[derive(Debug)]
pub enum BackupError {
    Io(io::Error),
    Store(WalBackedError),
    UnstableSource,
    RestoreMismatch,
    UnsafeDestination,
    Metadata(String),
    InvalidBackup(&'static str),
}

impl From<io::Error> for BackupError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}
impl From<WalBackedError> for BackupError {
    fn from(error: WalBackedError) -> Self {
        Self::Store(error)
    }
}
impl fmt::Display for BackupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "backup I/O failure: {e}"),
            Self::Store(e) => write!(f, "backup source validation failed: {e}"),
            Self::UnstableSource => f.write_str("backup source changed or WAL owner is poisoned"),
            Self::RestoreMismatch => {
                f.write_str("restored snapshot does not match captured owner state")
            }
            Self::UnsafeDestination => f.write_str(
                "backup destination must be a new directory under a trusted non-symlink parent",
            ),
            Self::Metadata(e) => write!(f, "backup manifest serialization failed: {e}"),
            Self::InvalidBackup(reason) => write!(f, "backup integrity check failed: {reason}"),
        }
    }
}
impl std::error::Error for BackupError {}

fn sync_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        File::open(path)?.sync_all()?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn private_directory(path: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)
}

fn new_private_file(path: &Path) -> io::Result<File> {
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)
}

fn hash_file(path: &Path) -> io::Result<(u64, String)> {
    let mut src = File::open(path)?;
    let mut total = 0u64;
    let mut digest = blake3::Hasher::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let read = src.read(&mut buf)?;
        if read == 0 {
            break;
        }
        total = total
            .checked_add(read as u64)
            .ok_or_else(|| io::Error::other("backup size overflow"))?;
        digest.update(&buf[..read]);
    }
    Ok((total, digest.finalize().to_hex().to_string()))
}

fn copy_with_hash(source: &Path, dest: &Path) -> io::Result<(u64, String)> {
    let mut src = File::open(source)?;
    let mut out = new_private_file(dest)?;
    let mut total = 0u64;
    let mut digest = blake3::Hasher::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let read = src.read(&mut buf)?;
        if read == 0 {
            break;
        }
        out.write_all(&buf[..read])?;
        digest.update(&buf[..read]);
        total = total
            .checked_add(read as u64)
            .ok_or_else(|| io::Error::other("backup size overflow"))?;
    }
    out.sync_all()?;
    Ok((total, digest.finalize().to_hex().to_string()))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BackupFileMetadata {
    bytes: u64,
    blake3: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OptionalBackupFileMetadata {
    bytes: Option<u64>,
    blake3: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BackupManifest {
    format: String,
    sequence: u64,
    tablet_id: Option<u64>,
    ownership_epoch: Option<u64>,
    checkpoint_sequence: Option<u64>,
    wal: BackupFileMetadata,
    checkpoint: OptionalBackupFileMetadata,
    proof: String,
}

fn require_regular_file(path: &Path) -> Result<u64, BackupError> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() {
        return Err(BackupError::InvalidBackup("non-regular backup component"));
    }
    Ok(metadata.len())
}

/// Audit an already-completed, immutable, single-tablet local backup without
/// mutating its WAL, checkpoint, completion marker, or lock sidecars.
///
/// Requires a trusted, quiescent local directory. It rejects symlinks in the
/// supplied directory and immediate components, but is NOT safe against an
/// attacker concurrently replacing ancestor directories or file entries.
/// BLAKE3 provides accidental-corruption detection, NOT source authentication:
/// an attacker able to rewrite files and the marker can forge both.
/// This checks the captured file hashes, metadata and WAL/checkpoint structure;
/// it does NOT rerun the capture-time logical state comparison, prove all WAL
/// and checkpoint overlap, or replace a disposable restore rehearsal.
pub fn audit_completed_backup_read_only(
    descriptor: TabletDescriptor,
    directory: impl AsRef<Path>,
) -> Result<BackupReceipt, BackupError> {
    const MAX_MARKER_BYTES: u64 = 16 * 1024;
    let directory = directory.as_ref();
    if !fs::symlink_metadata(directory)?.file_type().is_dir() {
        return Err(BackupError::InvalidBackup(
            "backup directory is not a real directory",
        ));
    }

    let marker_path = directory.join("COMPLETE.json");
    let marker_len = require_regular_file(&marker_path)?;
    if marker_len == 0 || marker_len > MAX_MARKER_BYTES {
        return Err(BackupError::InvalidBackup("invalid completion marker size"));
    }
    let mut marker = Vec::with_capacity(marker_len as usize);
    File::open(&marker_path)?
        .take(MAX_MARKER_BYTES + 1)
        .read_to_end(&mut marker)?;
    if marker.len() as u64 != marker_len {
        return Err(BackupError::InvalidBackup(
            "completion marker changed during read",
        ));
    }
    let document: serde_json::Value = serde_json::from_slice(&marker)
        .map_err(|error| BackupError::Metadata(error.to_string()))?;
    for name in [
        "format",
        "sequence",
        "tablet_id",
        "ownership_epoch",
        "checkpoint_sequence",
        "wal",
        "checkpoint",
        "proof",
    ] {
        if document.get(name).is_none() {
            return Err(BackupError::InvalidBackup(
                "required manifest field missing",
            ));
        }
    }
    for name in ["bytes", "blake3"] {
        if document
            .get("checkpoint")
            .and_then(|c| c.get(name))
            .is_none()
        {
            return Err(BackupError::InvalidBackup(
                "checkpoint manifest field missing",
            ));
        }
    }
    let manifest: BackupManifest = serde_json::from_value(document)
        .map_err(|error| BackupError::Metadata(error.to_string()))?;
    if manifest.format != "NUDBBACKUP1"
        || manifest.proof != "disposable-full-mvcc-and-receipt-match"
    {
        return Err(BackupError::InvalidBackup("unsupported backup manifest"));
    }

    let wal_path = directory.join("tablet.wal");
    if require_regular_file(&wal_path)? != manifest.wal.bytes
        || hash_file(&wal_path)? != (manifest.wal.bytes, manifest.wal.blake3.clone())
    {
        return Err(BackupError::InvalidBackup("WAL size or digest mismatch"));
    }

    let checkpoint_path = directory.join("tablet.checkpoint");
    let checkpoint = match (
        manifest.checkpoint.bytes,
        manifest.checkpoint.blake3.as_ref(),
    ) {
        (None, None) => {
            match fs::symlink_metadata(&checkpoint_path) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
                Ok(_) => return Err(BackupError::InvalidBackup("unexpected checkpoint file")),
            }
            None
        }
        (Some(bytes), Some(digest)) => {
            if require_regular_file(&checkpoint_path)? != bytes
                || hash_file(&checkpoint_path)? != (bytes, digest.clone())
            {
                return Err(BackupError::InvalidBackup(
                    "checkpoint size or digest mismatch",
                ));
            }
            Some((bytes, digest.clone()))
        }
        _ => return Err(BackupError::InvalidBackup("incomplete checkpoint manifest")),
    };

    let report =
        super::store::WalBackedTablet::verify_checkpoint_wal_read_only(descriptor, &wal_path)?;
    if report.durable_tail != manifest.sequence
        || report.checkpoint_sequence != manifest.checkpoint_sequence
        || report.wal.tablet_id.map(|id| id.get()) != manifest.tablet_id
        || report.wal.last_ownership_epoch != manifest.ownership_epoch
    {
        return Err(BackupError::InvalidBackup(
            "WAL or checkpoint metadata mismatch",
        ));
    }

    Ok(BackupReceipt {
        destination: directory.to_path_buf(),
        sequence: manifest.sequence,
        wal_bytes: manifest.wal.bytes,
        checkpoint_bytes: checkpoint.as_ref().map(|(bytes, _)| *bytes),
        wal_blake3: manifest.wal.blake3,
        checkpoint_blake3: checkpoint.map(|(_, digest)| digest),
    })
}

/// Capture, restore-proof and publish a new backup directory while the caller
/// holds the WAL sidecar writer lock and an immutable borrow of its owner.
///
/// The destination MUST NOT exist, including as a symlink. Its parent must
/// already exist and be trusted against concurrent path replacement. A
/// failed operation may leave a directory without COMPLETE.json: operators
/// must never treat that incomplete directory as a valid backup.
pub(crate) fn capture_locked(
    source_wal: &Path,
    source_checkpoint: &Path,
    descriptor: TabletDescriptor,
    source_report: CheckpointWalReport,
    destination: &Path,
    verify_disposable_restore: impl FnOnce(&Path) -> Result<(), BackupError>,
) -> Result<BackupReceipt, BackupError> {
    let parent = destination
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    if !parent.is_dir() || fs::symlink_metadata(parent)?.file_type().is_symlink() {
        return Err(BackupError::UnsafeDestination);
    }
    if destination.exists() || fs::symlink_metadata(destination).is_ok() {
        return Err(BackupError::UnsafeDestination);
    }
    // Exclusive first creation prevents any overwrite. Only this new, private
    // directory is ever populated and used for disposable proof.
    private_directory(destination)?;

    let wal_destination = destination.join("tablet.wal");
    let (wal_bytes, wal_blake3) = copy_with_hash(source_wal, &wal_destination)?;
    #[cfg(test)]
    super::interruption::hit(super::interruption::StorageInterruptionPoint::BackupAfterWalStaged)?;

    let checkpoint_destination = destination.join("tablet.checkpoint");
    let (checkpoint_bytes, checkpoint_blake3) = if source_checkpoint.exists() {
        let (bytes, digest) = copy_with_hash(source_checkpoint, &checkpoint_destination)?;
        (Some(bytes), Some(digest))
    } else {
        (None, None)
    };
    #[cfg(test)]
    super::interruption::hit(
        super::interruption::StorageInterruptionPoint::BackupAfterCheckpointStaged,
    )?;

    let expected_wal = (wal_bytes, wal_blake3.clone());
    if hash_file(source_wal)? != expected_wal {
        return Err(BackupError::UnstableSource);
    }
    if let (Some(bytes), Some(checksum)) = (checkpoint_bytes, checkpoint_blake3.as_ref()) {
        if hash_file(source_checkpoint)? != (bytes, checksum.clone()) {
            return Err(BackupError::UnstableSource);
        }
    } else if source_checkpoint.exists() {
        return Err(BackupError::UnstableSource);
    }

    // The staged file pair must independently satisfy structural admission.
    // The caller also verifies the *exact logical state* in a disposable
    // recovery directory rather than assuming checksums imply equivalence.
    let staged = super::store::WalBackedTablet::verify_checkpoint_wal_read_only(
        descriptor,
        &wal_destination,
    )?;
    if staged != source_report {
        return Err(BackupError::UnstableSource);
    }
    // Restore proof is required even when checkpoint/WAL sequence bounds match.
    let proof_dir = destination.join(".restore-proof");
    private_directory(&proof_dir)?;
    let proof_wal = proof_dir.join("tablet.wal");
    copy_with_hash(&wal_destination, &proof_wal)?;
    if checkpoint_bytes.is_some() {
        copy_with_hash(
            &checkpoint_destination,
            &proof_dir.join("tablet.checkpoint"),
        )?;
    }
    verify_disposable_restore(&proof_wal)?;
    // The proof folder is ours, was freshly created under the private output,
    // and is removed only after its recovery handle has been dropped.
    fs::remove_dir_all(&proof_dir)?;
    #[cfg(test)]
    super::interruption::hit(
        super::interruption::StorageInterruptionPoint::BackupAfterRestoreProof,
    )?;

    sync_directory(destination)?;
    let receipt = BackupReceipt {
        destination: destination.to_path_buf(),
        sequence: source_report.durable_tail,
        wal_bytes,
        checkpoint_bytes,
        wal_blake3,
        checkpoint_blake3,
    };
    let manifest = serde_json::to_vec_pretty(&serde_json::json!({
        "format": "NUDBBACKUP1",
        "sequence": receipt.sequence,
        "tablet_id": source_report.wal.tablet_id.map(|id| id.get()),
        "ownership_epoch": source_report.wal.last_ownership_epoch,
        "checkpoint_sequence": source_report.checkpoint_sequence,
        "wal": {"bytes": receipt.wal_bytes, "blake3": receipt.wal_blake3},
        "checkpoint": {
            "bytes": receipt.checkpoint_bytes,
            "blake3": receipt.checkpoint_blake3
        },
        "proof": "disposable-full-mvcc-and-receipt-match"
    }))
    .map_err(|error| BackupError::Metadata(error.to_string()))?;

    let temp = destination.join("COMPLETE.tmp");
    let mut file = new_private_file(&temp)?;
    file.write_all(&manifest)?;
    file.sync_all()?;
    drop(file);
    sync_directory(destination)?;
    #[cfg(test)]
    super::interruption::hit(
        super::interruption::StorageInterruptionPoint::BackupBeforeCompletion,
    )?;

    // The directory itself was created exclusively by this call and remains
    // private (0700), so no other writer can create COMPLETE.json in it.
    fs::rename(&temp, destination.join("COMPLETE.json"))?;
    sync_directory(destination)?;
    sync_directory(parent)?;
    Ok(receipt)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::interruption::{with_interruption, StorageInterruptionPoint};
    use crate::database::store::WalBackedTablet;
    use crate::database::tablet::{KeyRange, TabletDescriptor, TabletId, TabletMutation};
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(1);

    fn root(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "nudb_backup_interruption_{name}_{}_{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        path
    }

    fn descriptor() -> TabletDescriptor {
        TabletDescriptor::new(
            TabletId::new(127).unwrap(),
            KeyRange::new(b"a".to_vec(), Some(b"z".to_vec())).unwrap(),
            5,
        )
        .unwrap()
    }

    fn committed_tablet(root: &Path) -> WalBackedTablet {
        let mut db = WalBackedTablet::open(descriptor(), root.join("live.wal")).unwrap();
        let mutation = vec![TabletMutation::Put {
            key: b"k".to_vec(),
            value: b"secret-value".to_vec(),
        }];
        let write = db
            .prepare_write(5, db.current_sequence(), mutation)
            .unwrap();
        db.commit(write).unwrap();
        db.publish_checkpoint().unwrap();
        db
    }

    #[test]
    fn interruptions_never_publish_a_complete_backup_or_mutate_sources() {
        for point in [
            StorageInterruptionPoint::BackupAfterWalStaged,
            StorageInterruptionPoint::BackupAfterCheckpointStaged,
            StorageInterruptionPoint::BackupAfterRestoreProof,
            StorageInterruptionPoint::BackupBeforeCompletion,
        ] {
            let dir = root("staged");
            let db = committed_tablet(&dir);
            let wal = dir.join("live.wal");
            let checkpoint = wal.with_extension("checkpoint");
            let wal_before = fs::read(&wal).unwrap();
            let checkpoint_before = fs::read(&checkpoint).unwrap();
            let bad = dir.join("interrupted");
            let outcome = with_interruption(point, || db.capture_consistent_backup(&bad));
            assert!(outcome.is_err(), "failure must propagate for {point:?}");
            assert!(
                !bad.join("COMPLETE.json").exists(),
                "interrupted {point:?} cannot publish a completion marker"
            );
            assert_eq!(fs::read(&wal).unwrap(), wal_before);
            assert_eq!(fs::read(&checkpoint).unwrap(), checkpoint_before);
            assert_eq!(db.current_sequence(), 1);

            // An interrupted export cannot poison an otherwise healthy owner.
            let clean = dir.join("retry-new-target");
            let receipt = db.capture_consistent_backup(&clean).unwrap();
            assert_eq!(receipt.sequence, 1);
            assert!(clean.join("COMPLETE.json").is_file());
            drop(db);
            fs::remove_dir_all(&dir).unwrap();
        }
    }

    #[test]
    fn ambiguous_wal_commit_poison_blocks_backup_without_creating_destination() {
        let dir = root("poison");
        let mut db = WalBackedTablet::open(descriptor(), dir.join("live.wal")).unwrap();
        let mutation = vec![TabletMutation::Put {
            key: b"k".to_vec(),
            value: b"not-acknowledged".to_vec(),
        }];
        let write = db.prepare_write(5, 0, mutation).unwrap();
        let result = with_interruption(StorageInterruptionPoint::WalAfterPayload, || {
            db.commit(write)
        });
        assert!(result.is_err());
        let dest = dir.join("must-not-exist");
        let error = db.capture_consistent_backup(&dest).unwrap_err();
        assert!(matches!(error, BackupError::UnstableSource));
        assert!(!dest.exists());
        drop(db);
        fs::remove_dir_all(dir).unwrap();
    }
}
