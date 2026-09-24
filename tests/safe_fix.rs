//! End-to-end contracts for transactional machine-applicable fixes.

use std::path::{Path, PathBuf};
use std::process::Command;

fn nulang_exe() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_nulang"))
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "nulang_safe_fix_{}_{}_{}",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn write_typo(dir: &Path) -> PathBuf {
    let path = dir.join("bad.nula");
    std::fs::write(&path, "fn main(counter: Int) = countr + 1\n").expect("write source");
    path
}

fn run_fix(path: &Path, extra: &[&str]) -> std::process::Output {
    let mut command = Command::new(nulang_exe());
    command.args(["fix", "--safe"]).args(extra).arg(path);
    command.output().expect("run safe fix")
}

#[test]
fn dry_run_reports_edit_without_mutating_source() {
    let dir = temp_dir("dry_run");
    let path = write_typo(&dir);
    let before = std::fs::read_to_string(&path).unwrap();

    let out = run_fix(&path, &["--dry-run", "--json"]);
    assert!(
        out.status.success(),
        "safe fix dry-run failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).expect("json report");
    assert_eq!(report["changed"], true);
    assert_eq!(report["dry_run"], true);
    assert_eq!(report["applied_fixes"], 1);
    assert_eq!(report["applied_edits"], 1);
    assert_eq!(report["errors_after"], 0);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), before);

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn safe_fix_applies_then_becomes_idempotent() {
    let dir = temp_dir("apply");
    let path = write_typo(&dir);

    let first = run_fix(&path, &["--json"]);
    assert!(
        first.status.success(),
        "safe fix failed: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    let first_report: serde_json::Value =
        serde_json::from_slice(&first.stdout).expect("first json report");
    assert_eq!(first_report["changed"], true);
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "fn main(counter: Int) = counter + 1\n"
    );

    let check = Command::new(nulang_exe())
        .arg("--check")
        .arg(&path)
        .output()
        .expect("check fixed source");
    assert!(
        check.status.success(),
        "fixed source must type-check: {}",
        String::from_utf8_lossy(&check.stderr)
    );

    let second = run_fix(&path, &["--json"]);
    assert!(second.status.success());
    let second_report: serde_json::Value =
        serde_json::from_slice(&second.stdout).expect("second json report");
    assert_eq!(second_report["changed"], false);
    assert_eq!(second_report["applied_edits"], 0);

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn safe_flag_is_required() {
    let dir = temp_dir("guard");
    let path = write_typo(&dir);

    let out = Command::new(nulang_exe())
        .arg("fix")
        .arg(&path)
        .output()
        .expect("run guarded fix");
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("require --safe"),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let _ = std::fs::remove_dir_all(dir);
}
