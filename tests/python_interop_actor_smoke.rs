//! Source-to-actor Python effects smoke tests.
//!
//! Unlike the Rust-only bridge tests, these compile actual .nula programs,
//! exercise spawn authority grants, and run the Python-enabled CLI.
//! These cases must not be built into the release-cli profile, which omits
//! embedded CPython.
#![cfg(feature = "python")]

use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

fn execute_source(name: &str, source: &str) -> Output {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("wall clock before Unix epoch")
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "nulang-python-actor-smoke-{}-{nonce}-{name}.nula",
        std::process::id()
    ));
    std::fs::write(&path, source).expect("create Python actor smoke input");
    let result = Command::new(env!("CARGO_BIN_EXE_nulang"))
        .arg("--with")
        .arg("os")
        .arg(&path)
        .output();
    let _ = std::fs::remove_file(&path);
    result.expect("run Python-enabled Nulang CLI")
}

fn assert_output(output: Output, expected: &str) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "Nulang source-to-actor Python smoke failed: stdout={stdout:?}; stderr={stderr:?}"
    );
    assert_eq!(stdout.trim(), expected, "unexpected Nulang stdout; stderr={stderr}");
}

#[test]
fn imports_and_calls_python_with_unicode_and_returned_string_handles() {
    let output = execute_source(
        "unicode",
        r#"
actor PythonProbe {
    behavior check() {
        perform Python.import("builtins")
        let unicode_count = perform Python.call("builtins", "len", "Olá 🐍")
        let python_text = perform Python.call("builtins", "str", 423)
        let text_count = perform Python.call("builtins", "len", python_text)
        unicode_count + text_count
    }
}
fn main() {
    let worker = spawn PythonProbe {} with [Python::import, Python::call]
    let answer = ask worker check()
    perform IO.print(answer)
}
"#,
    );
    assert_output(output, "8");
}

#[test]
fn reads_non_callable_python_module_attributes() {
    let output = execute_source(
        "attribute",
        r#"
actor PythonProbe {
    behavior pi() {
        perform Python.import("math")
        perform Python.get_attr("math", "pi")
    }
}
fn main() {
    let worker = spawn PythonProbe {} with [Python::import, Python::get_attr]
    let pi = ask worker pi()
    perform IO.print(perform Float.to_string(pi))
}
"#,
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "Python attribute smoke failed: stdout={stdout:?}; stderr={stderr:?}"
    );
    assert!(
        stdout.trim().starts_with("3.14159"),
        "expected math.pi, got stdout={stdout:?}; stderr={stderr:?}"
    );
}
