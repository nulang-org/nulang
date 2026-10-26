//! Experimental single-host durable file sink for NuDB receipt-managed
//! consumers. An immutable, checksum-protected effect record is the concrete
//! downstream side effect. Its file and parent directory are fsynced before
//! returning a receipt to the local retention registry.
//!
//! Assumes a trusted local filesystem with cooperative exclusive file locking.
//! No distributed exactly-once, remote acknowledgment authentication, power-loss
//! proof, adversarial concurrent pathname replacement, or automated recovery of
//! partially written sink records. Never delete a live owner lock sidecar.
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::retention::RetentionClass;
use super::retention_store::{DurableRetentionRegistry, RegistryError};

const MAGIC: &[u8; 8] = b"NUDBFX01";
const MAX_EFFECT_BYTES: usize = 64 * 1024;
const MAX_RECORD_BYTES: u64 = 1024 * 1024;

#[derive(Debug)]
pub enum SinkError {
    Io(io::Error),
    Registry(RegistryError),
    UnsafePath,
    WriterLocked,
    InvalidDelivery,
    Conflict,
    Corrupt,
}
impl fmt::Display for SinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "file sink I/O: {e}"),
            Self::Registry(e) => write!(f, "durable registry rejected delivery: {e}"),
            Self::UnsafePath => f.write_str("unsafe file sink alias or directory"),
            Self::WriterLocked => f.write_str("file sink already has an exclusive owner"),
            Self::InvalidDelivery => {
                f.write_str("invalid file sink consumer, id, cursor or effect")
            }
            Self::Conflict => f.write_str("delivery id already contains different durable effect"),
            Self::Corrupt => f.write_str("durable file sink record invalid, incomplete or corrupt"),
        }
    }
}
impl std::error::Error for SinkError {}
impl From<io::Error> for SinkError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}
impl From<RegistryError> for SinkError {
    fn from(e: RegistryError) -> Self {
        Self::Registry(e)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct EffectRecord {
    version: u8,
    class: RetentionClass,
    consumer: String,
    delivery_id: String,
    next_needed: u64,
    effect: Vec<u8>,
}

fn valid_identifier(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b':' | b'.'))
}
fn valid_fields(class: RetentionClass, consumer: &str, id: &str, cursor: u64) -> bool {
    class != RetentionClass::Snapshot
        && valid_identifier(consumer)
        && valid_identifier(id)
        && cursor > 0
}
fn safe_file(path: &Path) -> Result<bool, SinkError> {
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            if !meta.file_type().is_file() {
                return Err(SinkError::UnsafePath);
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                if meta.nlink() != 1 {
                    return Err(SinkError::UnsafePath);
                }
            }
            Ok(true)
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(SinkError::Io(e)),
    }
}
fn sync_directory(root: &Path) -> io::Result<()> {
    File::open(root)?.sync_all()
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
fn encode(rec: &EffectRecord) -> Result<Vec<u8>, SinkError> {
    let payload = serde_json::to_vec(rec).map_err(|_| SinkError::InvalidDelivery)?;
    if payload.len() as u64 > MAX_RECORD_BYTES - 44 {
        return Err(SinkError::InvalidDelivery);
    }
    let mut bytes = Vec::with_capacity(payload.len() + 44);
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&payload);
    bytes.extend_from_slice(blake3::hash(&payload).as_bytes());
    Ok(bytes)
}
fn decode(bytes: &[u8]) -> Result<EffectRecord, SinkError> {
    if bytes.len() < 44 || bytes.len() as u64 > MAX_RECORD_BYTES || &bytes[..8] != MAGIC {
        return Err(SinkError::Corrupt);
    }
    let n = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
    if n + 44 != bytes.len() {
        return Err(SinkError::Corrupt);
    }
    let payload = &bytes[12..12 + n];
    if bytes[12 + n..] != *blake3::hash(payload).as_bytes() {
        return Err(SinkError::Corrupt);
    }
    let rec: EffectRecord = serde_json::from_slice(payload).map_err(|_| SinkError::Corrupt)?;
    if rec.version != 1
        || !valid_fields(rec.class, &rec.consumer, &rec.delivery_id, rec.next_needed)
        || rec.effect.len() > MAX_EFFECT_BYTES
    {
        return Err(SinkError::Corrupt);
    }
    Ok(rec)
}

/// Exclusively owned immutable local sink. A committed file is never
/// rewritten, so stable delivery IDs provide crash-replay idempotence.
/// Do not unlink '.sink-owner.lock' while any handle could be alive.
#[derive(Debug)]
pub struct DurableFileSink {
    root: PathBuf,
    _lock: File,
}
impl DurableFileSink {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, SinkError> {
        #[cfg(not(unix))]
        {
            let _ = path;
            return Err(SinkError::UnsafePath);
        }
        #[cfg(unix)]
        {
            let requested = path.as_ref();
            let meta = fs::symlink_metadata(requested)?;
            if !meta.file_type().is_dir() {
                return Err(SinkError::UnsafePath);
            }
            use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
            if meta.mode() & 0o022 != 0 {
                return Err(SinkError::UnsafePath);
            }
            let root = fs::canonicalize(requested)?;
            let lock_path = root.join(".sink-owner.lock");
            safe_file(&lock_path)?;
            let mut opts = OpenOptions::new();
            opts.read(true).write(true).create(true).mode(0o600);
            let lock = opts.open(&lock_path)?;
            safe_file(&lock_path)?;
            lock.try_lock().map_err(|e| match e {
                fs::TryLockError::WouldBlock => SinkError::WriterLocked,
                fs::TryLockError::Error(error) => SinkError::Io(error),
            })?;
            sync_directory(&root)?;
            Ok(Self { root, _lock: lock })
        }
    }

    /// Name depends only on (class, consumer, id), not requested cursor or
    /// effect: an attempted ID reuse is detected rather than silently routed
    /// to a new file.
    pub fn record_name(class: RetentionClass, consumer: &str, delivery_id: &str) -> String {
        let key = serde_json::to_vec(&(class, consumer, delivery_id))
            .expect("serializing a fixed tuple cannot fail");
        format!("effect-{}.bin", blake3::hash(&key).to_hex())
    }

    fn read_record(
        &self,
        class: RetentionClass,
        consumer: &str,
        id: &str,
    ) -> Result<Option<(EffectRecord, Vec<u8>)>, SinkError> {
        let path = self.root.join(Self::record_name(class, consumer, id));
        if !safe_file(&path)? {
            return Ok(None);
        }
        let mut file = File::open(&path)?;
        if file.metadata()?.len() > MAX_RECORD_BYTES {
            return Err(SinkError::Corrupt);
        }
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        let rec = decode(&bytes)?;
        if rec.class != class || rec.consumer != consumer || rec.delivery_id != id {
            return Err(SinkError::Corrupt);
        }
        // Crash after writing but before directory fsync: verifying and
        // syncing the same immutable record makes a retry durable.
        file.sync_all()?;
        sync_directory(&self.root)?;
        Ok(Some((rec, bytes)))
    }

    /// Durably apply the local effect once; receipt bytes are a content hash
    /// of the complete checksummed record, not a proof of remote delivery.
    pub fn apply(
        &self,
        class: RetentionClass,
        consumer: &str,
        next_needed: u64,
        delivery_id: &str,
        effect: &[u8],
    ) -> Result<[u8; 32], SinkError> {
        if !valid_fields(class, consumer, delivery_id, next_needed)
            || effect.len() > MAX_EFFECT_BYTES
        {
            return Err(SinkError::InvalidDelivery);
        }
        let rec = EffectRecord {
            version: 1,
            class,
            consumer: consumer.to_owned(),
            delivery_id: delivery_id.to_owned(),
            next_needed,
            effect: effect.to_vec(),
        };
        if let Some((existing, bytes)) = self.read_record(class, consumer, delivery_id)? {
            if existing != rec {
                return Err(SinkError::Conflict);
            }
            return Ok(*blake3::hash(&bytes).as_bytes());
        }
        let bytes = encode(&rec)?;
        let path = self
            .root
            .join(Self::record_name(class, consumer, delivery_id));
        // No overwrite or stage rename: if power fails mid-write, an
        // incomplete record remains corrupt and blocks unsafe acknowledgement.
        let mut file = match new_private_file(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                let (existing, previous) = self
                    .read_record(class, consumer, delivery_id)?
                    .ok_or(SinkError::Corrupt)?;
                if existing != rec {
                    return Err(SinkError::Conflict);
                }
                return Ok(*blake3::hash(&previous).as_bytes());
            }
            Err(e) => return Err(e.into()),
        };
        file.write_all(&bytes)?;
        file.sync_all()?;
        sync_directory(&self.root)?;
        Ok(*blake3::hash(&bytes).as_bytes())
    }

    pub fn read_effect(
        &self,
        class: RetentionClass,
        consumer: &str,
        delivery_id: &str,
    ) -> Result<Option<Vec<u8>>, SinkError> {
        if !valid_identifier(consumer) || !valid_identifier(delivery_id) {
            return Err(SinkError::InvalidDelivery);
        }
        Ok(self
            .read_record(class, consumer, delivery_id)?
            .map(|(rec, _)| rec.effect))
    }

    /// End-to-end *local* adapter: WAL pin cannot advance before the immutable
    /// sink effect file AND its directory entry have been durably synced.
    /// External effect durability and remote exactly-once are not implied.
    pub fn deliver_and_ack(
        &self,
        registry: &mut DurableRetentionRegistry,
        class: RetentionClass,
        consumer: &str,
        next_needed: u64,
        delivery_id: &str,
        effect: &[u8],
    ) -> Result<u64, SinkError> {
        // Reject structurally invalid deliveries without leaving a durable
        // pending pin that an operator would have to reconcile.
        if !valid_fields(class, consumer, delivery_id, next_needed)
            || effect.len() > MAX_EFFECT_BYTES
        {
            return Err(SinkError::InvalidDelivery);
        }
        registry.begin_delivery(class, consumer, next_needed, delivery_id)?;
        let receipt = self.apply(class, consumer, next_needed, delivery_id, effect)?;
        Ok(registry.commit_delivery(class, consumer, next_needed, delivery_id, &receipt)?)
    }
}
