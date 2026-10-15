//! A prebuilt Nulang CLI must run its package tools without Cargo or Rust.
//!
//! This is intentionally an integration test of the *installed binary* path.
//! In particular, do not set CARGO_BIN_EXE_nulang in the child's environment:
//! doing so masks failures in package::commands::try_find_existing_nulang_binary.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

struct DisposableProject(PathBuf);

impl DisposableProject {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "nulang_standalone_cli_{}_{}",
            std::process::id(),
            nonce
        ));
        fs::create_dir_all(dir.join("no-external-tools")).expect("create isolated project");
        Self(dir)
    }
}

impl Drop for DisposableProject {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn run_without_external_toolchain(exe: &Path, cwd: &Path, tools: &Path, args: &[&str]) -> Output {
    Command::new(exe)
        .args(args)
        .current_dir(cwd)
        .env_remove("CARGO_BIN_EXE_nulang")
        .env("PATH", tools)
        .env(
            "NULANG_STDLIB",
            Path::new(env!("CARGO_MANIFEST_DIR")).join("src/stdlib"),
        )
        .output()
        .expect("launch prebuilt nulang executable")
}

fn assert_success(output: &Output, command: &str) {
    assert!(
        output.status.success(),
        "{command} failed without Cargo (exit {:?})\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn prebuilt_cli_scaffolds_builds_runs_and_tests_without_cargo() {
    let exe = PathBuf::from(env!("CARGO_BIN_EXE_nulang"));
    let temp = DisposableProject::new();
    let no_tools = temp.0.join("no-external-tools");

    let created = run_without_external_toolchain(
        &exe,
        &temp.0,
        &no_tools,
        &["nula", "new", "standalone-smoke", "--template", "lib"],
    );
    assert_success(&created, "nula new");

    let project = temp.0.join("standalone-smoke");
    let built = run_without_external_toolchain(&exe, &project, &no_tools, &["nula", "build"]);
    assert_success(&built, "nula build");
    assert!(
        project.join(".nula/dist/standalone-smoke.nbc").is_file(),
        "nula build must produce the compiled package artifact"
    );

    let ran = run_without_external_toolchain(&exe, &project, &no_tools, &["nula", "run"]);
    assert_success(&ran, "nula run");
    assert!(
        String::from_utf8_lossy(&ran.stdout).contains("Library package ready."),
        "nula run must execute the package entry point"
    );

    let tested = run_without_external_toolchain(&exe, &project, &no_tools, &["nula", "test"]);
    assert_success(&tested, "nula test");
    assert!(
        String::from_utf8_lossy(&tested.stdout).contains("test result: 1 passed; 0 failed"),
        "nula test must discover and execute the library template test; output: {}",
        String::from_utf8_lossy(&tested.stdout)
    );
}
