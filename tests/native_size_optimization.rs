//! End-to-end contract for the optional native code-size optimization policy.
//! The native backend still executes from a JIT module; these tests do not
//! claim that --native-opt creates a standalone object or executable.

#![cfg(feature = "native-codegen")]

use std::process::Command;

#[test]
fn native_small_optimization_preserves_program_result() {
    let output = Command::new(env!("CARGO_BIN_EXE_nulang"))
        .args([
            "--backend",
            "native",
            "--native-opt",
            "small",
            "--eval",
            "1 + 2 * 3",
        ])
        .output()
        .expect("run Nulang native backend");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.lines().any(|line| line.trim() == "7"),
        "unexpected stdout: {stdout}"
    );
}

#[test]
fn native_optimization_rejects_unknown_mode() {
    let output = Command::new(env!("CARGO_BIN_EXE_nulang"))
        .args([
            "--backend",
            "native",
            "--native-opt",
            "invalid",
            "--eval",
            "1 + 2",
        ])
        .output()
        .expect("run Nulang CLI");
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("speed | small"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn native_optimization_must_not_silently_apply_to_bytecode() {
    let output = Command::new(env!("CARGO_BIN_EXE_nulang"))
        .args([
            "--backend",
            "bytecode",
            "--native-opt",
            "small",
            "--eval",
            "1 + 2",
        ])
        .output()
        .expect("run Nulang CLI");
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("requires --backend native"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
