//! Crash-durable filesystem ordering for grain identity claims.
//!
//! `grain_identity::claim_json_identity` makes the identity file contents
//! durable and prevents overwrite races. On POSIX filesystems, creation of a
//! new directory entry additionally requires syncing the containing directory;
//! if the actor directory was itself just created, its parent must be synced as
//! well. Durable state must not be written until this wrapper succeeds.

use crate::grain_identity::{claim_json_identity, PersistedGrainIdentity};
use crate::runtime::GrainId;
use std::io;
use std::path::Path;

#[cfg(unix)]
fn sync_directory(path: &Path) -> io::Result<()> {
    std::fs::File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> io::Result<()> {
    // Rust/OS directory-sync support differs on non-Unix platforms. The file
    // contents are still sync_all'd by the lower-level claim. Platform-specific
    // directory durability should be implemented before advertising equivalent
    // power-loss guarantees there.
    Ok(())
}

/// Claim and durably publish a JSON grain identity sidecar before any actor
/// snapshot/journal state is allowed into the same compact-id namespace.
///
/// Ordering on Unix:
/// 1. exclusive `create_new` identity claim;
/// 2. write identity bytes;
/// 3. `sync_all` the identity file (inside `claim_json_identity`);
/// 4. `sync_all` the actor directory so the sidecar directory entry is durable;
/// 5. `sync_all` the actor directory's parent so a newly-created actor directory
///    is itself durably linked into the store;
/// 6. only then may caller persist actor state.
pub(crate) fn claim_json_identity_durable(
    path: &Path,
    requested: &GrainId,
) -> io::Result<PersistedGrainIdentity> {
    let record = claim_json_identity(path, requested)?;

    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        sync_directory(parent)?;
        if let Some(grandparent) = parent.parent().filter(|p| !p.as_os_str().is_empty()) {
            sync_directory(grandparent)?;
        }
    }

    Ok(record)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grain_identity::load_json_identity;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_identity_path(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir()
            .join(format!(
                "nulang-grain-durable-{label}-{}-{nonce}",
                std::process::id()
            ))
            .join("actor_7")
            .join("grain_identity.json")
    }

    #[test]
    fn durable_claim_creates_and_validates_identity() {
        let path = temp_identity_path("create");
        let grain = GrainId::new("User", "42");
        let record = claim_json_identity_durable(&path, &grain).unwrap();

        assert_eq!(load_json_identity(&path).unwrap(), Some(record));
        let _ = fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn durable_claim_remains_idempotent() {
        let path = temp_identity_path("idempotent");
        let grain = GrainId::new("User", "42");
        let first = claim_json_identity_durable(&path, &grain).unwrap();
        let second = claim_json_identity_durable(&path, &grain).unwrap();
        assert_eq!(first, second);

        let _ = fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn durable_wrapper_preserves_fail_closed_mismatch() {
        let path = temp_identity_path("mismatch");
        let stored = GrainId::new("User", "42");
        claim_json_identity_durable(&path, &stored).unwrap();
        let before = fs::read(&path).unwrap();

        let error = claim_json_identity_durable(&path, &GrainId::new("Order", "42"))
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(fs::read(&path).unwrap(), before);

        let _ = fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
    }
}
