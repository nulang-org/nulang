//! Agent-safe project inspection contract. Run with:
 //! cargo test --test nula_inspect

use std::path::{Path, PathBuf};
use std::process::Command;

fn run(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_nulang"))
        .args(args)
        .current_dir(dir)
        .output()
        .expect("nulang CLI must launch")
}

fn fresh_dir() -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "nula_inspect_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&path).unwrap();
    path
}

#[test]
fn inspect_json_reports_only_whitelisted_project_metadata() {
    let dir = fresh_dir();
    std::fs::write(
        dir.join("Nulang.toml"),
        r#"[package]
name = "demo-agent"
version = "0.1.0"
capabilities = ["net"]
registry = "https://user:private-token@example.org"

[dependencies]
helper = { git = "https://user:another-secret@example.org/helper.git" }
"#,
    ).unwrap();
    std::fs::write(dir.join("Nulang.lock"), "").unwrap();

    let out = run(&dir, &["nula", "inspect", "--json"]);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8(out.stdout).unwrap();
    let value: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(value["schema_version"], 1);
    assert_eq!(value["command"], "inspect");
    assert_eq!(value["project"]["name"], "demo-agent");
    assert_eq!(value["project"]["version"], "0.1.0");
    assert_eq!(value["project"]["entry"], "src/main.nula");
    assert_eq!(value["project"]["capabilities"], serde_json::json!(["net"]));
    assert_eq!(value["project"]["dependencies"], serde_json::json!(["helper"]));
    assert_eq!(value["project"]["lockfile_present"], true);
    assert!(!stdout.contains("private-token"));
    assert!(!stdout.contains("another-secret"));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn inspect_fails_without_manifest_and_rejects_unknown_flags() {
    let dir = fresh_dir();
    let missing = run(&dir, &["nula", "inspect", "--json"]);
    assert!(!missing.status.success());
    assert!(missing.stdout.is_empty());
    std::fs::write(
        dir.join("Nulang.toml"),
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
    ).unwrap();
    let unknown = run(&dir, &["nula", "inspect", "--typo"]);
    assert!(!unknown.status.success());
    std::fs::remove_dir_all(&dir).unwrap();
}
