//! A concrete local-file acceptance sink for exercising the durable workflow
//! output-delivery contract, including cross-process key deduplication.
//!
//! Acceptance **is** the durable record: this does not send HTTP, execute a
//! payment, or atomically couple its ledger to a separate external service.
//! The implementation requires a trusted, local filesystem with POSIX-like
//! atomic hard-link creation and directory fsync semantics. It is not a
//! substitute for a real downstream provider's server-side deduplication.

use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Error, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const FORMAT_VERSION: u16 = 1;
const MAX_BODY_SIZE: usize = 64 * 1024;
const MAX_DESTINATION_SIZE: usize = 2048;
const MAX_RECORD_SIZE: u64 = 8 * 1024;
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone)]
pub struct LocalAcceptanceProvider {
    directory: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct AcceptedRecord {
    version: u16,
    key: String,
    destination: String,
    payload_digest: String,
    record_checksum: String,
}

impl AcceptedRecord {
    fn new(key: &str, destination: &str, payload: &[u8]) -> Self {
        let digest = blake3::hash(payload).to_hex().to_string();
        Self::from_digest(key, destination, &digest)
    }

    fn from_digest(key: &str, destination: &str, digest: &str) -> Self {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"nulang.local-acceptance.v1\0");
        for part in [key, destination, digest] {
            hasher.update(&(part.len() as u64).to_le_bytes());
            hasher.update(part.as_bytes());
        }
        Self {
            version: FORMAT_VERSION,
            key: key.to_owned(),
            destination: destination.to_owned(),
            payload_digest: digest.to_owned(),
            record_checksum: hasher.finalize().to_hex().to_string(),
        }
    }

    fn validate(&self) -> io::Result<()> {
        if self.version != FORMAT_VERSION
            || !is_valid_key(&self.key)
            || !is_valid_key(&self.payload_digest)
            || !valid_destination(&self.destination)
            || *self != Self::from_digest(&self.key, &self.destination, &self.payload_digest)
        {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "corrupted local acceptance record",
            ));
        }
        Ok(())
    }
}

fn is_valid_key(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn valid_destination(destination: &str) -> bool {
    !destination.trim().is_empty() && destination.len() <= MAX_DESTINATION_SIZE
}

struct TempRecord(PathBuf);
impl Drop for TempRecord {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

impl LocalAcceptanceProvider {
    /// Initialize a local acceptance directory. Keep it on a trusted, local
    /// filesystem; do not share it over a network file system with uncertain
    /// hard-link/directory-sync guarantees.
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        fs::create_dir_all(path.as_ref())?;
        let directory = fs::canonicalize(path)?;
        if !fs::metadata(&directory)?.is_dir() {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "acceptance path must be a directory",
            ));
        }
        Ok(Self { directory })
    }

    /// Persist the acceptance once, then return the *identical* result bytes
    /// on retry. A key reused for a different destination or payload fails
    /// closed, rather than reapplying the operation with new arguments.
    pub fn accept(&self, key: &str, destination: &str, payload: &[u8]) -> io::Result<Vec<u8>> {
        self.accept_inner(key, destination, payload, || Ok(()), || Ok(()))
    }

    fn accept_inner<B, A>(
        &self,
        key: &str,
        destination: &str,
        payload: &[u8],
        before_publish: B,
        after_publish: A,
    ) -> io::Result<Vec<u8>>
    where
        B: FnOnce() -> io::Result<()>,
        A: FnOnce() -> io::Result<()>,
    {
        if !is_valid_key(key) || !valid_destination(destination) || payload.len() > MAX_BODY_SIZE {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "invalid local acceptance key, destination or payload",
            ));
        }
        let expected = AcceptedRecord::new(key, destination, payload);
        let final_path = self.directory.join(format!("{key}.json"));
        if final_path.exists() {
            return self.read_existing(&final_path, &expected);
        }
        let bytes = serde_json::to_vec(&expected)
            .map_err(|error| Error::new(ErrorKind::InvalidData, error))?;
        if bytes.len() as u64 > MAX_RECORD_SIZE {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "local acceptance record too large",
            ));
        }

        // Write a *complete*, fsynced record in the same directory, and use
        // atomic hard_link() for create-if-absent publication. rename() would
        // incorrectly overwrite another process's receipt on duplicate keys.
        let (mut file, temporary) = self.create_temporary()?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        before_publish()?;
        match fs::hard_link(&temporary.0, &final_path) {
            Ok(()) => {
                after_publish()?;
                // Acknowledge success only after the directory entry is synced.
                self.sync_directory()?;
                Ok(bytes)
            }
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                self.read_existing(&final_path, &expected)
            }
            Err(error) => Err(error),
        }
        // `temporary` cleanup happens on drop, even on error or unwind.
    }

    fn create_temporary(&self) -> io::Result<(File, TempRecord)> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| Error::new(ErrorKind::Other, error))?
            .as_nanos();
        for _ in 0..16 {
            let serial = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = self
                .directory
                .join(format!(".pending-{}-{now}-{serial}", std::process::id()));
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => return Ok((file, TempRecord(path))),
                Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        Err(Error::new(
            ErrorKind::AlreadyExists,
            "could not allocate unique local acceptance temporary file",
        ))
    }

    fn read_existing(&self, path: &Path, expected: &AcceptedRecord) -> io::Result<Vec<u8>> {
        // Do not interpret corrupt/partial records as a missing operation.
        let metadata = fs::metadata(path)?;
        if !metadata.is_file() || metadata.len() > MAX_RECORD_SIZE {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "invalid local acceptance receipt file",
            ));
        }
        let bytes = fs::read(path)?;
        let stored: AcceptedRecord = serde_json::from_slice(&bytes)
            .map_err(|error| Error::new(ErrorKind::InvalidData, error))?;
        stored.validate()?;
        if serde_json::to_vec(&stored).map_err(|error| Error::new(ErrorKind::InvalidData, error))?
            != bytes
        {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "non-canonical local acceptance record",
            ));
        }
        if &stored != expected {
            return Err(Error::new(
                ErrorKind::AlreadyExists,
                "local acceptance key was previously committed with different request",
            ));
        }
        // Ensure the published directory entry is durably acknowledged on
        // recovery, including a prior writer that crashed before dir fsync.
        self.sync_directory()?;
        Ok(serde_json::to_vec(&stored)
            .map_err(|error| Error::new(ErrorKind::InvalidData, error))?)
    }

    fn sync_directory(&self) -> io::Result<()> {
        File::open(&self.directory)?.sync_all()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn temp() -> PathBuf {
        std::env::temp_dir().join(format!(
            "nulang-local-fault-{}-{}",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ))
    }
    fn key() -> String {
        "1".repeat(64)
    }

    #[test]
    fn crash_before_publish_does_not_acknowledge_or_create_committed_record() {
        let path = temp();
        let provider = LocalAcceptanceProvider::open(&path).unwrap();
        let error = provider
            .accept_inner(
                &key(),
                "local:one",
                b"data",
                || Err(Error::new(ErrorKind::Interrupted, "crash before publish")),
                || Ok(()),
            )
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Interrupted);
        assert_eq!(fs::read_dir(&path).unwrap().count(), 0);
        provider.accept(&key(), "local:one", b"data").unwrap();
        assert_eq!(fs::read_dir(&path).unwrap().count(), 1);
        fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn crash_after_publish_before_dir_sync_is_recovered_with_same_key() {
        let path = temp();
        let provider = LocalAcceptanceProvider::open(&path).unwrap();
        let error = provider
            .accept_inner(
                &key(),
                "local:one",
                b"data",
                || Ok(()),
                || Err(Error::new(ErrorKind::Interrupted, "crash after publish")),
            )
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Interrupted);
        let expected = provider.accept(&key(), "local:one", b"data").unwrap();
        assert!(!expected.is_empty());
        assert_eq!(fs::read_dir(&path).unwrap().count(), 1);
        fs::remove_dir_all(path).unwrap();
    }
}
