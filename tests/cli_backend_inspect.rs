//! CLI contract tests for compile-only backend inspection.
//! The command must not execute arbitrary input even when compilation succeeds.

use serde_json::Value;
use std::process::Command;

fn run_inspection(source: &str, backend: &str) -> (std::process::Output, Value) {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "nulang-inspect-{}-{unique}.nula",
        std::process::id()
    ));
    std::fs::write(&path, source).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_nulang"))
        .arg("--inspect")
        .arg("--backend")
        .arg(backend)
        .arg(&path)
        .output()
        .unwrap();
    let _ = std::fs::remove_file(&path);

    let stdout = String::from_utf8(output.stdout.clone()).unwrap();
    let report = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("expected a standalone JSON report, got {stdout:?}: {e}"));
    (output, report)
}

#[test]
fn inspection_is_compile_only_and_produces_versioned_json() {
    let (output, report) = run_inspection(
        r#"perform IO.print("DO_NOT_EXECUTE_INSPECTION")"#,
        "bytecode",
    );
    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(report["schema_version"], 1);
    assert_eq!(report["backend"], "bytecode");
    assert_eq!(report["status"], "compiles");
    assert_eq!(report["verification"], "compile_only");
    assert_eq!(report["diagnostics"], serde_json::json!([]));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("DO_NOT_EXECUTE_INSPECTION"));
}

#[test]
fn parse_failures_remain_machine_readable_and_fail_closed() {
    let (output, report) = run_inspection("fn (", "bytecode");
    assert!(!output.status.success());
    assert_eq!(report["status"], "rejected");
    assert_eq!(report["diagnostics"][0]["code"], "frontend.rejected");
}

#[test]
fn unknown_backends_cannot_be_reported_as_compiling() {
    let (output, report) = run_inspection("1 + 2", "not-a-backend");
    assert!(!output.status.success());
    assert_eq!(report["status"], "unavailable");
    assert_eq!(report["diagnostics"][0]["code"], "backend.unknown");
}
