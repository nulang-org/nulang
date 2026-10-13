//! End-to-end native object/linker smoke tests. Host-libc executable; Linux only.
//! These tests intentionally exercise the compiler CLI rather than the VM.

#![cfg(all(target_os = "linux", feature = "native-executable"))]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn test_dir() -> PathBuf {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path =
        std::env::temp_dir().join(format!("nulang-native-tiny-{}-{nonce}", std::process::id()));
    fs::create_dir(&path).unwrap();
    path
}

fn build(dir: &Path, source: &str) -> std::process::Output {
    let input = dir.join("main.nula");
    fs::write(&input, source).unwrap();
    Command::new(env!("CARGO_BIN_EXE_nulang"))
        .args(["--emit-exe", "--out"])
        .arg(dir.join("hello"))
        .arg(input)
        .output()
        .unwrap()
}

#[test]
fn hello_world_is_a_real_executable_without_the_nulang_runtime() {
    assert!(
        Command::new("cc").arg("--version").output().is_ok(),
        "native executable tests require a host C linker (cc)"
    );
    let dir = test_dir();
    let built = build(&dir, "perform IO.print(\"Hello from native Nulang\")");
    assert!(
        built.status.success(),
        "compiler stderr: {}",
        String::from_utf8_lossy(&built.stderr)
    );
    let program = dir.join("hello");
    assert!(program.exists());
    let run = Command::new(&program).output().unwrap();
    assert_eq!(run.status.code(), Some(0));
    assert_eq!(run.stdout, b"Hello from native Nulang\n");
    assert!(run.stderr.is_empty());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn unsupported_string_expression_fails_closed_without_an_executable() {
    let dir = test_dir();
    let built = build(
        &dir,
        "let name = \"Nulang\" + \"!\";
perform IO.print(name)",
    );
    assert!(!built.status.success());
    assert!(String::from_utf8_lossy(&built.stderr).contains("unsupported"));
    assert!(!dir.join("hello").exists());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn embedded_nul_is_rejected_without_truncating_output() {
    let dir = test_dir();
    let built = build(&dir, "perform IO.print(\"A\\0B\")");
    assert!(!built.status.success());
    assert!(!dir.join("hello").exists());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn multiple_prints_preserve_order_and_utf8() {
    let dir = test_dir();
    let built = build(
        &dir,
        "perform IO.print(\"First\");\nperform IO.print(\"Olá, Nulang\")",
    );
    assert!(
        built.status.success(),
        "compiler stderr: {}",
        String::from_utf8_lossy(&built.stderr)
    );
    let run = Command::new(dir.join("hello")).output().unwrap();
    assert_eq!(run.status.code(), Some(0));
    assert_eq!(run.stdout, "First\nOlá, Nulang\n".as_bytes());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn unsupported_source_preserves_existing_executable_bytes() {
    let dir = test_dir();
    let artifact = dir.join("hello");
    fs::write(&artifact, b"existing-good-artifact").unwrap();
    let built = build(
        &dir,
        "let greeting = \"Hello\" + \" World\";\nperform IO.print(greeting)",
    );
    assert!(!built.status.success());
    assert!(String::from_utf8_lossy(&built.stderr).contains("unsupported"));
    assert_eq!(fs::read(&artifact).unwrap(), b"existing-good-artifact");
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn integer_overflow_follows_48bit_vm_wraparound_without_failing() {
    // Nulang Int arithmetic wraps at 48 bits rather than trapping.
    let dir = test_dir();
    let built = build(
        &dir,
        "let max = 140737488355327;\nlet overflow = max + 1;\nperform IO.print(\"after wrap\")",
    );
    assert!(
        built.status.success(),
        "compiler stderr: {}",
        String::from_utf8_lossy(&built.stderr)
    );
    let run = Command::new(dir.join("hello")).output().unwrap();
    assert_eq!(run.status.code(), Some(0));
    assert_eq!(run.stdout, b"after wrap\n");
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn minimum_integer_negation_wraps_without_failing() {
    let dir = test_dir();
    let built = build(
        &dir,
        "let min = -140737488355328;\nlet overflow = -min;\nperform IO.print(\"after negation\")",
    );
    assert!(
        built.status.success(),
        "compiler stderr: {}",
        String::from_utf8_lossy(&built.stderr)
    );
    let run = Command::new(dir.join("hello")).output().unwrap();
    assert_eq!(run.status.code(), Some(0));
    assert_eq!(run.stdout, b"after negation\n");
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn literal_prints_match_bytecode_reference_output() {
    let dir = test_dir();
    let source = "perform IO.print(\"Olá, Nulang\");\nperform IO.println(\"Second line\")";
    let compiled = build(&dir, source);
    assert!(
        compiled.status.success(),
        "compiler stderr: {}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    let reference = Command::new(env!("CARGO_BIN_EXE_nulang"))
        .arg("--backend")
        .arg("bytecode")
        .arg(dir.join("main.nula"))
        .output()
        .unwrap();
    assert!(
        reference.status.success(),
        "reference VM stderr: {}",
        String::from_utf8_lossy(&reference.stderr)
    );
    let native = Command::new(dir.join("hello")).output().unwrap();
    assert!(native.status.success());
    assert_eq!(native.stdout, reference.stdout);
    assert_eq!(native.stderr, reference.stderr);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn explicit_output_symlink_to_source_is_rejected() {
    use std::os::unix::fs::symlink;

    let dir = test_dir();
    let input = dir.join("source.nula");
    let alias = dir.join("alias");
    let source = "perform IO.print(\"do not overwrite\")";
    fs::write(&input, source).unwrap();
    symlink(&input, &alias).unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_nulang"))
        .args(["--emit-exe", "--out"])
        .arg(&alias)
        .arg(&input)
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert_eq!(fs::read_to_string(&input).unwrap(), source);
    assert!(alias.symlink_metadata().unwrap().file_type().is_symlink());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn standalone_print_matches_bytecode_output() {
    let dir = test_dir();
    let source = "perform IO.print(\"bytecode/native parity\")";
    let built = build(&dir, source);
    assert!(
        built.status.success(),
        "compiler stderr: {}",
        String::from_utf8_lossy(&built.stderr)
    );
    let bytecode = Command::new(env!("CARGO_BIN_EXE_nulang"))
        .arg(dir.join("main.nula"))
        .output()
        .expect("run bytecode reference");
    let native = Command::new(dir.join("hello"))
        .output()
        .expect("run linked executable");
    assert!(bytecode.status.success());
    assert!(native.status.success());
    assert_eq!(
        native.stdout, bytecode.stdout,
        "bytecode/native stdout mismatch"
    );
    assert_eq!(
        native.stderr, bytecode.stderr,
        "bytecode/native stderr mismatch"
    );
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn explicit_output_equal_to_source_is_rejected_without_modification() {
    let dir = test_dir();
    let input = dir.join("source.nula");
    let source = "perform IO.print(\"keep source\")";
    fs::write(&input, source).unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_nulang"))
        .args(["--emit-exe", "--out"])
        .arg(&input)
        .arg(&input)
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert_eq!(fs::read_to_string(&input).unwrap(), source);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn extensionless_source_defaults_to_distinct_binary_name() {
    let dir = test_dir();
    let input = dir.join("script");
    let source = "perform IO.print(\"extensionless\")";
    fs::write(&input, source).unwrap();
    let built = Command::new(env!("CARGO_BIN_EXE_nulang"))
        .arg("--emit-exe")
        .arg(&input)
        .output()
        .unwrap();
    assert!(
        built.status.success(),
        "compiler stderr: {}",
        String::from_utf8_lossy(&built.stderr)
    );
    assert_eq!(fs::read_to_string(&input).unwrap(), source);
    let run = Command::new(dir.join("script.bin")).output().unwrap();
    assert!(run.status.success());
    assert_eq!(run.stdout, b"extensionless\n");
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn emit_exe_rejects_eval_instead_of_executing_it() {
    let dir = test_dir();
    let artifact = dir.join("accidental-executable");
    let output = Command::new(env!("CARGO_BIN_EXE_nulang"))
        .args(["--emit-exe", "--eval", "1 + 2", "--out"])
        .arg(&artifact)
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "--emit-exe --eval must fail closed"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("--emit-exe requires"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!artifact.exists());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn emit_exe_rejects_check_instead_of_silently_typechecking() {
    let dir = test_dir();
    let input = dir.join("source.nula");
    fs::write(&input, "1 + 2").unwrap();
    let artifact = dir.join("accidental-executable");
    let output = Command::new(env!("CARGO_BIN_EXE_nulang"))
        .args(["--emit-exe", "--check"])
        .arg(&input)
        .arg("--out")
        .arg(&artifact)
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "--emit-exe --check must fail closed"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("--emit-exe requires"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!artifact.exists());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn emit_exe_rejects_precompiled_bytecode_without_running_it() {
    let dir = test_dir();
    let input = dir.join("source.nbc");
    fs::write(&input, b"not-valid-bytecode").unwrap();
    let artifact = dir.join("accidental-executable");
    let output = Command::new(env!("CARGO_BIN_EXE_nulang"))
        .arg("--emit-exe")
        .arg(&input)
        .arg("--out")
        .arg(&artifact)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("source file"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!artifact.exists());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn dash_prefixed_output_directory_does_not_turn_linker_input_into_option() {
    // A relative path beginning with '-' must not be passed as a positional
    // linker argument: GCC/ld can treat it as a command-line option.
    let dir = test_dir();
    let output_dir = dir.join("-native-output");
    fs::create_dir(&output_dir).unwrap();
    fs::write(
        dir.join("main.nula"),
        "perform IO.print(\"safe linker path\")",
    )
    .unwrap();
    let built = Command::new(env!("CARGO_BIN_EXE_nulang"))
        .current_dir(&dir)
        .args(["--emit-exe", "--out", "-native-output/hello", "main.nula"])
        .output()
        .unwrap();
    assert!(
        built.status.success(),
        "compiler stderr: {}",
        String::from_utf8_lossy(&built.stderr)
    );
    let binary = output_dir.join("hello");
    let executed = Command::new(binary).output().unwrap();
    assert!(executed.status.success());
    assert_eq!(executed.stdout, b"safe linker path\n");
    fs::remove_dir_all(dir).unwrap();
}
