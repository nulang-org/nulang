//! Non-destructive Linux resource-ceiling proof for NuDB's WAL append boundary.
//! RLIMIT_FSIZE causes EFBIG during an actual kernel write after partial WAL
//! progress. This is NOT ENOSPC, OOM, or physical-power-loss certification.
#![cfg(target_os = "linux")]

use nulang::database::store::{WalBackedError, WalBackedTablet};
use nulang::database::tablet::{
    KeyRange, TabletDescriptor, TabletId, TabletMutation, TransactionId,
};
use nulang::database::wal::WalError;
use std::fs;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(1);
const FIRST_ID: [u8; 16] = [0x71; 16];
const SECOND_ID: [u8; 16] = [0x72; 16];

struct Sandbox(PathBuf);
impl Sandbox {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "nudb_kernel_limit_{}_{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let mut builder = fs::DirBuilder::new();
        builder.mode(0o700);
        builder.create(&p).unwrap();
        Self(p)
    }
    fn wal(&self) -> PathBuf {
        self.0.join("tablet.wal")
    }
}
impl Drop for Sandbox {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

fn descriptor() -> TabletDescriptor {
    TabletDescriptor::new(
        TabletId::new(912).unwrap(),
        KeyRange::new(b"a".to_vec(), Some(b"z".to_vec())).unwrap(),
        7,
    )
    .unwrap()
}

fn put(key: &[u8], value: &[u8]) -> Vec<TabletMutation> {
    vec![TabletMutation::Put {
        key: key.to_vec(),
        value: value.to_vec(),
    }]
}

// Runs ONLY when spawned by the parent; the RLIMIT is set AFTER the initial
// durable commit and applies to this child process, never the test runner.
#[test]
fn resource_limited_writer_child_fixture() {
    let Some(path) = std::env::var_os("NUDB_RLIMIT_CHILD_WAL") else {
        return;
    };
    let path = PathBuf::from(path);
    let mut tablet = WalBackedTablet::open(descriptor(), &path).unwrap();
    let first = tablet
        .commit_idempotent(TransactionId::new(FIRST_ID), put(b"k", b"before"))
        .unwrap();
    assert_eq!(first.sequence(), 1);

    let length = fs::metadata(&path).unwrap().len();
    let ceiling = length + 12; // Partial frame: less than a complete header.
    install_fsize_limit(ceiling);
    let outcome =
        tablet.commit_idempotent(TransactionId::new(SECOND_ID), put(b"k", &vec![b'x'; 2048]));
    match outcome {
        Err(WalBackedError::Wal(WalError::Io { kind, .. })) => {
            assert_eq!(kind, std::io::ErrorKind::FileTooLarge);
        }
        other => panic!("expected genuine Linux EFBIG from the kernel, got {other:?}"),
    }
    assert_eq!(
        fs::metadata(&path).unwrap().len(),
        ceiling,
        "kernel must have accepted a partial WAL frame before rejecting the rest"
    );

    assert_eq!(
        tablet.current_sequence(),
        1,
        "no unpublished state may be exposed"
    );
    assert_eq!(tablet.read_latest(b"k"), Some(&b"before"[..]));
    assert!(
        matches!(
            tablet.commit_idempotent(TransactionId::new(SECOND_ID), put(b"k", b"retry")),
            Err(WalBackedError::Wal(WalError::Poisoned))
        ),
        "I/O failure must poison the live writer until recovery"
    );
}

fn install_fsize_limit(max_bytes: u64) {
    // This is a disposable child process, so lowering its soft resource limit
    // cannot constrain the parallel test runner or other NuDB writers.
    let mut old = std::mem::MaybeUninit::<libc::rlimit>::uninit();
    assert_eq!(
        unsafe { libc::getrlimit(libc::RLIMIT_FSIZE, old.as_mut_ptr()) },
        0
    );
    let old = unsafe { old.assume_init() };
    assert!(
        max_bytes <= old.rlim_max,
        "fixture ceiling exceeds system hard limit"
    );
    let new = libc::rlimit {
        rlim_cur: max_bytes as libc::rlim_t,
        rlim_max: old.rlim_max,
    };
    assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_FSIZE, &new) }, 0);
    // Linux ordinarily signals SIGXFSZ on EFBIG. Ignore it in the child so
    // write_all returns the kernel I/O error instead of terminating the test.
    assert_ne!(
        unsafe { libc::signal(libc::SIGXFSZ, libc::SIG_IGN) },
        libc::SIG_ERR
    );
}

fn run_limited_writer(wal: &Path) {
    let result = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "resource_limited_writer_child_fixture",
            "--nocapture",
        ])
        .env("NUDB_RLIMIT_CHILD_WAL", wal)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "resource-limit child failed: status={} stdout={} stderr={}",
        result.status,
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}

#[test]
fn kernel_write_ceiling_poison_and_reopen_drops_torn_unacknowledged_frame() {
    let sandbox = Sandbox::new();
    run_limited_writer(&sandbox.wal());

    // Read-only inspection cannot silently truncate a torn source; only a
    // fresh owner may perform repair after the original child has exited.
    let torn_bytes = fs::metadata(sandbox.wal()).unwrap().len();
    assert!(WalBackedTablet::verify_checkpoint_wal_read_only(descriptor(), sandbox.wal()).is_err());
    assert_eq!(
        fs::metadata(sandbox.wal()).unwrap().len(),
        torn_bytes,
        "read-only audit must never truncate the original WAL"
    );
    let mut reopened = WalBackedTablet::open(descriptor(), sandbox.wal()).unwrap();
    assert!(
        fs::metadata(sandbox.wal()).unwrap().len() < torn_bytes,
        "a new exclusive owner must discard the incomplete WAL frame"
    );
    assert_eq!(reopened.current_sequence(), 1);
    assert_eq!(reopened.read_at(b"k", 1).unwrap(), Some(&b"before"[..]));
    let first = reopened
        .commit_idempotent(TransactionId::new(FIRST_ID), put(b"k", b"before"))
        .unwrap();
    assert_eq!(first.sequence(), 1, "receipt survives resource exhaustion");
    let second = reopened
        .commit_idempotent(TransactionId::new(SECOND_ID), put(b"k", b"recovered"))
        .unwrap();
    assert_eq!(second.sequence(), 2);
    assert_eq!(reopened.read_latest(b"k"), Some(&b"recovered"[..]));
}
