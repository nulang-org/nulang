//! Process-kill proof: acceptance remains readable from disk after the
//! accepting *process* is forcibly terminated, not just a Rust object drop.
#![cfg(unix)]
use nulang::workflow_local_provider::LocalAcceptanceProvider;
use std::{
    fs, io,
    process::Command,
    thread,
    time::{Duration, Instant},
};

const KEY: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

#[test]
fn process_worker_after_acceptance() {
    let Ok(root) = std::env::var("NULANG_TEST_ACCEPT_ROOT") else {
        return;
    };
    let provider = LocalAcceptanceProvider::open(&root).unwrap();
    let receipt = provider
        .accept(KEY, "local:process-kill", b"confirmed")
        .unwrap();
    assert!(!receipt.is_empty());
    // The parent kills the worker after this marker; unlike an in-process
    // restart, its memory and file handles are no longer available.
    fs::write(format!("{root}/.worker-ready"), b"ready").unwrap();
    loop {
        thread::sleep(Duration::from_secs(1));
    }
}

#[test]
fn accepted_operation_survives_sigkill_and_new_process_reopen() -> io::Result<()> {
    let unique = format!(
        "nulang-sigkill-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let root = std::env::temp_dir().join(unique);
    fs::create_dir_all(&root)?;
    let mut child = Command::new(std::env::current_exe()?)
        .arg("--exact")
        .arg("process_worker_after_acceptance")
        .arg("--nocapture")
        .env("NULANG_TEST_ACCEPT_ROOT", &root)
        .spawn()?;
    let deadline = Instant::now() + Duration::from_secs(30);
    while !root.join(".worker-ready").exists() {
        if let Some(status) = child.try_wait()? {
            return Err(io::Error::other(format!(
                "provider worker unexpectedly exited: {status}"
            )));
        }
        if Instant::now() > deadline {
            child.kill()?;
            child.wait()?;
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "provider worker did not commit",
            ));
        }
        thread::sleep(Duration::from_millis(20));
    }
    child.kill()?; // Unix Child::kill sends SIGKILL.
    let status = child.wait()?;
    assert!(!status.success());
    let provider = LocalAcceptanceProvider::open(&root)?;
    let recovered = provider.accept(KEY, "local:process-kill", b"confirmed")?;
    assert!(!recovered.is_empty());
    assert_eq!(
        provider
            .accept(KEY, "local:process-kill", b"other")
            .unwrap_err()
            .kind(),
        io::ErrorKind::AlreadyExists
    );
    fs::remove_file(root.join(".worker-ready"))?;
    assert_eq!(
        fs::read_dir(&root)?.count(),
        1,
        "one accepted operation after worker SIGKILL"
    );
    fs::remove_dir_all(root)?;
    Ok(())
}
