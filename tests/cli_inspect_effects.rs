//! Regression tests for non-executing, compiler-owned effect-row introspection.
//! Effects here are static upper bounds, never evidence of runtime execution.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn source(tag: &str, contents: &str) -> (PathBuf, PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "nulang_inspect_effects_{}_{}_{}",
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

fn run(flags: &[&str], path: &Path) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_nulang"));
    command.env(
        "NULANG_STDLIB",
        Path::new(env!("CARGO_MANIFEST_DIR")).join("src/stdlib"),
    );
    if let Some(index) = flags.iter().position(|flag| *flag == "--check") {
        command.args(&flags[..=index]).arg(path).args(&flags[index + 1..]);
    } else {
        command.args(flags).arg(path);
    }
    command.output().unwrap()
}

#[test]
fn effects_include_transitive_calls_and_never_execute_program() {
    let (dir, file) = source(
        "transitive",
        "fn leaf() { perform IO.print(\"SIDE_EFFECT_MARKER\") }\nfn caller() = leaf()\n",
    );
    let output = run(&["--check", "--inspect-effects"], &file);
    assert!(
        output.status.success(),
        "effect inspection should pass: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("=== Inferred Function Effects ==="), "{stdout}");
    assert!(stdout.contains("leaf: {IO}"), "{stdout}");
    assert!(stdout.contains("caller: {IO}"), "{stdout}");
    assert!(
        !stdout.contains("SIDE_EFFECT_MARKER"),
        "introspection must not execute IO: {stdout}"
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn pure_effect_rows_are_printed_in_stable_name_order() {
    let (dir, file) = source("pure", "fn z() = 1\nfn a() = 2\n");
    let output = run(&["--inspect-effects", "--check"], &file);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let stdout = String::from_utf8(output.stdout).unwrap();
    let a = stdout.find("a: {}").expect("a has empty effect row");
    let z = stdout.find("z: {}").expect("z has empty effect row");
    assert!(a < z, "effects should be ordered by declaration name: {stdout}");
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn type_and_effect_introspection_can_be_requested_together() {
    let (dir, file) = source("combined", "fn answer() -> Int = 42\n");
    let output = run(&["--check", "--inspect-types", "--inspect-effects"], &file);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("=== Inferred Declaration Types ==="), "{stdout}");
    assert!(stdout.contains("=== Inferred Function Effects ==="), "{stdout}");
    assert!(stdout.contains("answer: {}"), "{stdout}");
    assert!(!stdout.contains("Type check passed."), "{stdout}");
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn rejects_json_and_does_not_emit_partial_reports_on_failed_check() {
    let (dir, ok) = source("json", "fn main() = 1\n");
    let mixed = run(&["--check", "--inspect-effects", "--json"], &ok);
    assert!(!mixed.status.success());
    assert!(String::from_utf8_lossy(&mixed.stderr).contains("--json"));
    std::fs::remove_dir_all(dir).unwrap();

    let (dir, broken) = source("broken", "fn main() = missing_variable\n");
    let failed = run(&["--check", "--inspect-effects"], &broken);
    assert!(!failed.status.success());
    assert!(!String::from_utf8_lossy(&failed.stdout).contains("Inferred Function Effects"));
    std::fs::remove_dir_all(dir).unwrap();
}
