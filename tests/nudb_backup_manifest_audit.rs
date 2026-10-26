use nulang::database::backup::audit_completed_backup_read_only;
use nulang::database::store::WalBackedTablet;
use nulang::database::tablet::{
    KeyRange, TabletDescriptor, TabletId, TabletMutation, TransactionId,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(1);

struct Sandbox(PathBuf);
impl Sandbox {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "nudb_backup_audit_{}_{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        Self(root)
    }
    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}
impl Drop for Sandbox {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

fn descriptor() -> TabletDescriptor {
    TabletDescriptor::new(
        TabletId::new(121).unwrap(),
        KeyRange::new(b"a".to_vec(), Some(b"z".to_vec())).unwrap(),
        4,
    )
    .unwrap()
}

fn make_backup(sandbox: &Sandbox, with_checkpoint: bool) -> PathBuf {
    let mut tablet = WalBackedTablet::open(descriptor(), sandbox.path("owner.wal")).unwrap();
    let mutations = vec![TabletMutation::Put {
        key: b"k".to_vec(),
        value: b"private-value".to_vec(),
    }];
    tablet
        .commit_idempotent(TransactionId::new([7; 16]), mutations)
        .unwrap();
    if with_checkpoint {
        tablet.checkpoint().unwrap();
    }
    let backup = sandbox.path("backup");
    tablet.capture_consistent_backup(&backup).unwrap();
    backup
}

fn assert_rejected_unchanged(directory: &Path) {
    let before_wal = fs::read(directory.join("tablet.wal")).unwrap();
    let before_manifest = fs::symlink_metadata(directory.join("COMPLETE.json")).unwrap();
    assert!(audit_completed_backup_read_only(descriptor(), directory).is_err());
    assert_eq!(fs::read(directory.join("tablet.wal")).unwrap(), before_wal);
    assert_eq!(
        fs::symlink_metadata(directory.join("COMPLETE.json"))
            .unwrap()
            .len(),
        before_manifest.len()
    );
    assert!(!directory.join("tablet.wal.lock").exists());
}

#[test]
fn valid_wal_only_backup_is_audited_without_mutating_files() {
    let root = Sandbox::new();
    let dir = make_backup(&root, false);
    let before = fs::read(dir.join("tablet.wal")).unwrap();
    let audit = audit_completed_backup_read_only(descriptor(), &dir).unwrap();
    assert_eq!(audit.destination, dir);
    assert_eq!(audit.sequence, 1);
    assert_eq!(audit.checkpoint_bytes, None);
    assert_eq!(audit.wal_bytes as usize, before.len());
    assert_eq!(fs::read(dir.join("tablet.wal")).unwrap(), before);
    assert!(!dir.join("tablet.wal.lock").exists());
}

#[test]
fn valid_reclaimed_checkpoint_backup_is_audited_read_only() {
    let root = Sandbox::new();
    let dir = make_backup(&root, true);
    let checkpoint = fs::read(dir.join("tablet.checkpoint")).unwrap();
    let audit = audit_completed_backup_read_only(descriptor(), &dir).unwrap();
    assert_eq!(audit.sequence, 1);
    assert_eq!(audit.checkpoint_bytes, Some(checkpoint.len() as u64));
    assert_eq!(fs::read(dir.join("tablet.checkpoint")).unwrap(), checkpoint);
}

#[test]
fn missing_completion_marker_must_not_be_trusted() {
    let root = Sandbox::new();
    let dir = make_backup(&root, false);
    fs::remove_file(dir.join("COMPLETE.json")).unwrap();
    assert!(audit_completed_backup_read_only(descriptor(), &dir).is_err());
    assert!(!dir.join("tablet.wal.lock").exists());
}

#[test]
fn modified_wal_with_same_byte_length_fails_audit() {
    let root = Sandbox::new();
    let dir = make_backup(&root, false);
    let wal = dir.join("tablet.wal");
    let mut bytes = fs::read(&wal).unwrap();
    let last = bytes.last_mut().unwrap();
    *last ^= 0x01;
    fs::write(&wal, bytes).unwrap();
    assert_rejected_unchanged(&dir);
}

#[test]
fn forged_manifest_sequence_and_hash_fail_closed() {
    for field in ["sequence", "blake3", "format"] {
        let root = Sandbox::new();
        let dir = make_backup(&root, false);
        let marker = dir.join("COMPLETE.json");
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(&marker).unwrap()).unwrap();
        match field {
            "sequence" => manifest["sequence"] = serde_json::json!(99999),
            "blake3" => manifest["wal"]["blake3"] = serde_json::json!("00"),
            "format" => manifest["format"] = serde_json::json!("NOT_NUDB"),
            _ => unreachable!(),
        }
        fs::write(&marker, serde_json::to_vec(&manifest).unwrap()).unwrap();
        assert_rejected_unchanged(&dir);
    }
}

#[test]
fn wrong_tablet_descriptor_fails_audit() {
    let root = Sandbox::new();
    let dir = make_backup(&root, false);
    let wrong = TabletDescriptor::new(
        TabletId::new(122).unwrap(),
        KeyRange::new(b"a".to_vec(), Some(b"z".to_vec())).unwrap(),
        4,
    )
    .unwrap();
    assert!(audit_completed_backup_read_only(wrong, &dir).is_err());
}

#[test]
fn inconsistent_checkpoint_presence_fails_audit() {
    let root = Sandbox::new();
    let dir = make_backup(&root, true);
    fs::remove_file(dir.join("tablet.checkpoint")).unwrap();
    assert_rejected_unchanged(&dir);

    let root = Sandbox::new();
    let dir = make_backup(&root, false);
    fs::write(dir.join("tablet.checkpoint"), b"unmanifested").unwrap();
    assert_rejected_unchanged(&dir);
}

#[cfg(unix)]
#[test]
fn symlinked_marker_or_wal_is_rejected_without_following_it() {
    use std::os::unix::fs::symlink;
    for file in ["COMPLETE.json", "tablet.wal"] {
        let root = Sandbox::new();
        let dir = make_backup(&root, false);
        let real = root.path(file);
        fs::rename(dir.join(file), &real).unwrap();
        symlink(&real, dir.join(file)).unwrap();
        assert!(audit_completed_backup_read_only(descriptor(), &dir).is_err());
        assert!(!dir.join("tablet.wal.lock").exists());
    }
}
