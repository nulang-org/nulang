//! End-to-end contracts for ergonomic aliases and the optional beginner prelude.
//! Legacy commands and explicit effect syntax must remain available.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn nulang_exe() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_nulang"))
}

fn temp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "nulang_dx_{}_{}_{}",
        label,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("create temp directory");
    dir
}

fn command(root: &Path, args: &[&str]) -> Output {
    Command::new(nulang_exe())
        .args(args)
        .current_dir(root)
        .env(
            "NULANG_STDLIB",
            Path::new(env!("CARGO_MANIFEST_DIR")).join("src/stdlib"),
        )
        .output()
        .expect("run nulang CLI")
}

#[test]
fn check_alias_matches_existing_json_check() {
    let root = temp_dir("check");
    let file = root.join("sample.nula");
    std::fs::write(&file, "fn main() = 42\n").expect("write source");
    let file_arg = file.to_str().expect("utf-8 path");

    let old = command(&root, &["--check", file_arg, "--json"]);
    let alias = command(&root, &["check", file_arg, "--json"]);
    assert!(old.status.success(), "legacy check failed: {old:?}");
    assert_eq!(alias.status, old.status);
    assert_eq!(alias.stdout, old.stdout, "JSON result must be identical");
    assert_eq!(alias.stderr, old.stderr, "error stream must be identical");

    std::fs::remove_dir_all(root).expect("remove temp directory");
}

#[test]
fn new_alias_creates_same_package_structure_as_nula_new() {
    let root = temp_dir("new");
    let direct = root.join("simple-app");
    let legacy = root.join("legacy-app");
    let direct_arg = direct.to_str().expect("utf-8 path");
    let legacy_arg = legacy.to_str().expect("utf-8 path");

    let alias = command(&root, &["new", direct_arg]);
    let old = command(&root, &["nula", "new", legacy_arg]);
    assert!(old.status.success(), "legacy package command failed: {old:?}");
    assert!(alias.status.success(), "new alias failed: {alias:?}");
    for relative in ["Nulang.toml", "src/main.nula"] {
        assert!(direct.join(relative).exists(), "alias missing {relative}");
        assert!(legacy.join(relative).exists(), "legacy missing {relative}");
    }

    std::fs::remove_dir_all(root).expect("remove temp directory");
}

#[test]
fn test_alias_preserves_package_failure_semantics() {
    // An empty directory is intentionally not a package. Both spellings
    // must fail identically without creating a project or masking errors.
    let root = temp_dir("test");
    let old = command(&root, &["nula", "test"]);
    let alias = command(&root, &["test"]);
    assert!(!old.status.success(), "invalid package unexpectedly passed");
    assert_eq!(alias.status, old.status);
    assert_eq!(alias.stdout, old.stdout);
    assert_eq!(alias.stderr, old.stderr);
    std::fs::remove_dir_all(root).expect("remove temp directory");
}

#[test]
fn explicit_prelude_wraps_effects_without_new_language_semantics() {
    let root = temp_dir("prelude");
    let file = root.join("main.nula");
    std::fs::write(
        &file,
        "import stdlib::prelude\nfn main() {\n    perform Test.assert_eq(string_length(\"abc\"), 3)\n    println(int_to_string(42))\n}\n",
    )
    .expect("write source");
    let output = command(&root, &[file.to_str().expect("utf-8 path")]);
    assert!(output.status.success(), "prelude program failed: {output:?}");
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("42"),
        "prelude must produce normal IO output: {output:?}"
    );
    std::fs::remove_dir_all(root).expect("remove temp directory");
}
