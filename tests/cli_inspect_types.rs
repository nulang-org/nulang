//! Read-only compiler introspection: inferred top-level declaration types.
//! Regression coverage for Nulang's Julia-inspired type-inspection CLI.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn temp_source(tag: &str, contents: &str) -> (PathBuf, PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "nulang_inspect_types_{}_{}_{}",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("create temporary test directory");
    let path = dir.join("main.nula");
    std::fs::write(&path, contents).expect("write test source");
    (dir, path)
}

fn run(args: &[&str], file: Option<&Path>) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_nulang"));
    command.args(args).env(
        "NULANG_STDLIB",
        Path::new(env!("CARGO_MANIFEST_DIR")).join("src/stdlib"),
    );
    if let Some(path) = file {
        command.arg(path);
    }
    command.output().expect("execute nulang")
}

#[test]
fn inspect_types_reports_inferred_declarations_in_name_order_without_running() {
    let (dir, file) = temp_source(
        "sorted",
        "fn z(x: Int) -> Int = x\nfn a(x: Int) -> Int = x\n",
    );
    let output = run(&["--inspect-types", "--check"], Some(&file));
    assert!(
        output.status.success(),
        "inspect should type-check: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 stdout");
    assert!(stdout.contains("Inferred Declaration Types"), "{stdout}");
    let a = stdout.find("a: Int -> Int").expect("inferred type for a");
    let z = stdout.find("z: Int -> Int").expect("inferred type for z");
    assert!(a < z, "declarations must be sorted by name: {stdout}");
    assert!(!stdout.contains("Type check passed."), "report replaces success message");
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn regular_check_output_remains_unchanged() {
    let (dir, file) = temp_source("default", "fn main() = 1\n");
    let output = run(&["--check"], Some(&file));
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().trim(),
        "Type check passed."
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn inspection_never_prints_a_partial_report_for_invalid_source() {
    let (dir, file) = temp_source("invalid", "fn main() = missing_variable\n");
    let output = run(&["--inspect-types", "--check"], Some(&file));
    assert!(!output.status.success());
    assert!(!String::from_utf8_lossy(&output.stdout).contains("Inferred Declaration Types"));
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn inspection_requires_check_and_cannot_mix_with_json_diagnostics() {
    let no_check = run(&["--inspect-types"], None);
    assert!(!no_check.status.success());
    assert!(String::from_utf8_lossy(&no_check.stderr).contains("--check"));

    let (dir, file) = temp_source("json", "fn main() = 1\n");
    let mixed = run(&["--inspect-types", "--json", "--check"], Some(&file));
    assert!(!mixed.status.success());
    assert!(String::from_utf8_lossy(&mixed.stderr).contains("--json"));
    std::fs::remove_dir_all(dir).unwrap();
}
