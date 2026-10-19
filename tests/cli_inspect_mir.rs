//! CLI regression tests for non-executing MIR structural introspection.
//! MIR local slots and statement counts are *not* allocation measurements.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn source(tag: &str, contents: &str) -> (PathBuf, PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "nulang_inspect_mir_{}_{}_{}",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("main.nula");
    std::fs::write(&path, contents).unwrap();
    (dir, path)
}

fn run(args: &[&str], path: Option<&Path>) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_nulang"));
    cmd.env(
        "NULANG_STDLIB",
        Path::new(env!("CARGO_MANIFEST_DIR")).join("src/stdlib"),
    );
    if let Some(path) = path {
        if let Some(index) = args.iter().position(|arg| *arg == "--check") {
            cmd.args(&args[..=index]).arg(path).args(&args[index + 1..]);
        } else {
            cmd.args(args).arg(path);
        }
    } else {
        cmd.args(args);
    }
    cmd.output().unwrap()
}

#[test]
fn reports_sorted_mir_structure_not_runtime_allocation_estimates() {
    let (dir, file) = source("sorted", "fn z() -> Int = 2\nfn a() -> Int = 1\n");
    let output = run(&["--inspect-mir", "--check"], Some(&file));
    assert!(
        output.status.success(),
        "MIR inspection: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("=== MIR Structure (pre-codegen) ==="), "{stdout}");
    let a = stdout.find("function a:").expect("MIR for a");
    let z = stdout.find("function z:").expect("MIR for z");
    assert!(a < z, "MIR functions must be sorted: {stdout}");
    assert!(stdout.contains("blocks="), "{stdout}");
    assert!(stdout.contains("locals="), "{stdout}");
    assert!(stdout.contains("statements="), "{stdout}");
    assert!(!stdout.contains("allocations="), "{stdout}");
    assert!(!stdout.contains("Type check passed."), "{stdout}");
    assert_eq!(
        std::fs::read_dir(&dir).unwrap().count(),
        1,
        "inspection must not emit build artifacts"
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn allows_combined_read_only_inspection() {
    let (dir, file) = source("combined", "fn answer() -> Int = 42\n");
    let output = run(
        &["--check", "--inspect-types", "--inspect-effects", "--inspect-mir"],
        Some(&file),
    );
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("=== Inferred Declaration Types ==="), "{stdout}");
    assert!(stdout.contains("=== Inferred Function Effects ==="), "{stdout}");
    assert!(stdout.contains("=== MIR Structure (pre-codegen) ==="), "{stdout}");
    assert!(!stdout.contains("Type check passed."), "{stdout}");
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn invalid_program_never_emits_partial_type_effect_or_mir_reports() {
    let (dir, file) = source("bad", "fn main() = unknown_identifier\n");
    let output = run(
        &["--check", "--inspect-types", "--inspect-effects", "--inspect-mir"],
        Some(&file),
    );
    assert!(!output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains("=== Inferred Declaration Types ==="), "{stdout}");
    assert!(!stdout.contains("=== Inferred Function Effects ==="), "{stdout}");
    assert!(!stdout.contains("=== MIR Structure (pre-codegen) ==="), "{stdout}");
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn requires_check_and_rejects_json_and_execution_modes() {
    let no_check = run(&["--inspect-mir"], None);
    assert!(!no_check.status.success());
    assert!(String::from_utf8_lossy(&no_check.stderr).contains("--check"));

    let (dir, file) = source("guards", "fn main() = 1\n");
    let json = run(&["--check", "--inspect-mir", "--json"], Some(&file));
    assert!(!json.status.success());
    assert!(String::from_utf8_lossy(&json.stderr).contains("--json"));
    assert!(json.stdout.is_empty());

    let eval = run(
        &[
            "--inspect-mir",
            "--check",
            "--eval",
            r#"perform IO.print("INSPECTION_EXECUTED")"#,
        ],
        Some(&file),
    );
    assert!(!eval.status.success());
    assert!(!String::from_utf8_lossy(&eval.stdout).contains("INSPECTION_EXECUTED"));
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn source_line_metadata_is_deterministic_across_runs() {
    let (dir, file) = source("source_lines", "fn z() -> Int = 2\nfn a() -> Int = 1\n");
    let first = run(&["--inspect-mir", "--check"], Some(&file));
    let second = run(&["--inspect-mir", "--check"], Some(&file));
    assert!(first.status.success(), "{}", String::from_utf8_lossy(&first.stderr));
    assert!(second.status.success(), "{}", String::from_utf8_lossy(&second.stderr));
    assert_eq!(first.stdout, second.stdout, "same input should produce stable IR metadata");
    assert!(String::from_utf8_lossy(&first.stdout).contains("source_lines="));
    std::fs::remove_dir_all(dir).unwrap();
}
