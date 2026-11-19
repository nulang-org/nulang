//! Keep compiler front-end traversals within ordinary Linux process stacks.
//! These inputs are valid language-conformance fixtures, not stress-only rejects.
#![cfg(target_os = "linux")]

use std::path::Path;
use std::process::Command;

fn run_with_eight_mib_stack(fixture: &str, check_only: bool, expected: &str) {
    let file = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("conformance")
        .join("behavior")
        .join(fixture);
    // The POSIX shell's ulimit constrains the *actual* Nulang process,
    // unlike RUST_MIN_STACK (which does not affect the main thread).
    let mut process = Command::new("sh");
    process
        .arg("-c")
        .arg("ulimit -s 8192 || exit 88; exec \"$@\"")
        .arg("nulang-stack-check")
        .arg(env!("CARGO_BIN_EXE_nulang"));
    if check_only {
        process.arg("--check");
    }
    let output = process
        .arg(&file)
        .output()
        .expect("run Nulang under a normal 8 MiB stack");
    assert!(
        output.status.success(),
        "{} (check_only={}) must not crash on an 8 MiB stack (status={}):\n{}",
        fixture,
        check_only,
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    if !check_only {
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), expected);
    }
}

#[test]
fn wide_let_bindings_typecheck_with_normal_linux_stack() {
    run_with_eight_mib_stack("spill_03_wide_spill_correct_sum.nula", true, "");
    run_with_eight_mib_stack("spill_03_wide_spill_correct_sum.nula", false, "9870");
}

#[test]
fn deeply_nested_lets_typecheck_with_normal_linux_stack() {
    run_with_eight_mib_stack("spill_04_nested_lets_correct_sum.nula", true, "");
    run_with_eight_mib_stack("spill_04_nested_lets_correct_sum.nula", false, "7750");
}

#[test]
fn six_hundred_flat_bindings_typecheck_and_execute_under_eight_mib_stack() {
    run_with_eight_mib_stack(
        "spill_05_large_function_compiler_stack_overflow.nula",
        true,
        "",
    );
    run_with_eight_mib_stack(
        "spill_05_large_function_compiler_stack_overflow.nula",
        false,
        "600",
    );
}
