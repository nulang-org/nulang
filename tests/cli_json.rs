//! Integration tests for `--json` machine-readable diagnostics.
//!
//! Contract under test:
//! - `nulang --check <file> --json` emits exactly one JSON object on stdout
//!   (no human rendering mixed in), with `schema_version == 1`.
//! - Exit codes are unchanged (nonzero on errors).
//! - `nula build --json` / `nula test --json` follow the same schema.

use std::path::{Path, PathBuf};
use std::process::Command;

fn nulang_exe() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_nulang"))
}

fn stdlib_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src/stdlib")
}

fn write_temp(dir: &Path, name: &str, contents: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, contents).expect("write temp source");
    path
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "nulang_cli_json_{}_{}_{}",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

/// Run `nulang --check <file> --json`, returning (exit code, stdout, stderr).
fn run_check_json(file: &Path) -> (i32, String, String) {
    let out = Command::new(nulang_exe())
        .args(["--json", "--check"])
        .arg(file)
        .env("NULANG_STDLIB", stdlib_dir())
        .output()
        .expect("run nulang --check --json");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8(out.stdout).expect("stdout utf8"),
        String::from_utf8(out.stderr).expect("stderr utf8"),
    )
}

#[test]
fn check_json_unbound_variable_reports_e0202() {
    let dir = temp_dir("unbound");
    let src = write_temp(&dir, "bad.nula", "fn main() = countr + 1\n");

    let (code, stdout, _stderr) = run_check_json(&src);
    assert_ne!(code, 0, "failing check must exit nonzero");

    // stdout must be exactly one JSON object — no human rendering mixed in.
    let trimmed = stdout.trim();
    assert!(trimmed.starts_with('{'), "stdout must be JSON: {stdout:?}");
    assert!(trimmed.ends_with('}'), "stdout must be JSON: {stdout:?}");
    let v: serde_json::Value = serde_json::from_str(trimmed)
        .unwrap_or_else(|e| panic!("stdout must parse as JSON: {e}\n{stdout:?}"));

    assert_eq!(v["schema_version"], 1);
    assert_eq!(v["command"], "check");
    assert_eq!(v["ok"], false);
    let diags = v["diagnostics"].as_array().expect("diagnostics array");
    assert!(!diags.is_empty(), "expected at least one diagnostic");
    let d = &diags[0];
    assert_eq!(d["code"], "E0202", "unbound variable code");
    assert_eq!(d["severity"], "error");
    assert!(d["message"].as_str().expect("message").contains("countr"));
    let span = &d["span"];
    assert!(span.is_object(), "expected a resolved span: {d:?}");
    assert_eq!(span["line"], 1);
    assert!(span["col"].as_u64().unwrap() >= 1);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn check_json_parse_error_reports_e01xx() {
    let dir = temp_dir("parse");
    // Unclosed list literal → parse error (E0102-family).
    let src = write_temp(&dir, "bad.nula", "fn main() {\n    let x = [1, 2;\n}\n");

    let (code, stdout, _stderr) = run_check_json(&src);
    assert_ne!(code, 0, "parse error must exit nonzero");
    let v: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("stdout must parse as JSON: {e}\n{stdout:?}"));
    assert_eq!(v["schema_version"], 1);
    assert_eq!(v["ok"], false);
    let diags = v["diagnostics"].as_array().expect("diagnostics array");
    assert!(!diags.is_empty());
    let diag_code = diags[0]["code"].as_str().expect("error code");
    assert!(
        diag_code.starts_with("E01"),
        "parse errors carry E01xx codes, got {diag_code}"
    );
    assert_eq!(diags[0]["severity"], "error");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn check_json_passing_source_is_ok_with_empty_diagnostics() {
    let dir = temp_dir("pass");
    let src = write_temp(
        &dir,
        "ok.nula",
        "fn add(a: Int, b: Int) -> Int = a + b\nfn main() = add(1, 2)\n",
    );

    let (code, stdout, _stderr) = run_check_json(&src);
    assert_eq!(code, 0, "passing check must exit 0: {stdout:?}");
    let v: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("stdout must parse as JSON: {e}\n{stdout:?}"));
    assert_eq!(v["schema_version"], 1);
    assert_eq!(v["command"], "check");
    assert_eq!(v["ok"], true);
    assert_eq!(v["diagnostics"].as_array().unwrap().len(), 0);
    // No human rendering ("Type check passed.") may leak into stdout.
    assert!(!stdout.contains("Type check passed"));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn check_without_json_keeps_human_output() {
    let dir = temp_dir("human");
    let src = write_temp(&dir, "ok.nula", "fn main() = 1\n");

    let out = Command::new(nulang_exe())
        .args(["--check"])
        .arg(&src)
        .env("NULANG_STDLIB", stdlib_dir())
        .output()
        .expect("run nulang --check");
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.contains("Type check passed."),
        "human output must be unchanged without --json: {stdout:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Scaffold a minimal nula package in `dir`.
fn scaffold_package(dir: &Path, main_src: &str, tests: &[(&str, &str)]) {
    std::fs::write(
        dir.join("Nulang.toml"),
        "[package]\nname = \"jsonprobe\"\nversion = \"0.1.0\"\n",
    )
    .expect("write manifest");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/main.nula"), main_src).unwrap();
    std::fs::create_dir_all(dir.join("tests")).unwrap();
    for (name, src) in tests {
        std::fs::write(dir.join("tests").join(name), src).unwrap();
    }
}

fn run_nula(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new(nulang_exe())
        .arg("nula")
        .args(args)
        .current_dir(dir)
        .env("NULANG_STDLIB", stdlib_dir())
        .output()
        .expect("run nula")
}

#[test]
fn nula_build_json_success() {
    let dir = temp_dir("buildok");
    scaffold_package(&dir, "fn main() = 1\n", &[]);

    let out = run_nula(&dir, &["build", "--json"]);
    assert!(
        out.status.success(),
        "build should succeed: {:?}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    let v: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("stdout must parse as JSON: {e}\n{stdout:?}"));
    assert_eq!(v["schema_version"], 1);
    assert_eq!(v["command"], "build");
    assert_eq!(v["ok"], true);
    assert_eq!(v["diagnostics"].as_array().unwrap().len(), 0);
    assert!(!stdout.contains("Build succeeded"));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn nula_build_json_type_error_forwards_diagnostics() {
    let dir = temp_dir("buildbad");
    scaffold_package(&dir, "fn main() = countr + 1\n", &[]);

    let out = run_nula(&dir, &["build", "--json"]);
    assert!(!out.status.success(), "build must fail");
    let stdout = String::from_utf8(out.stdout).unwrap();
    let trimmed = stdout.trim();
    assert!(
        trimmed.starts_with('{') && trimmed.ends_with('}'),
        "stdout JSON only: {stdout:?}"
    );
    let v: serde_json::Value = serde_json::from_str(trimmed)
        .unwrap_or_else(|e| panic!("stdout must parse as JSON: {e}\n{stdout:?}"));
    assert_eq!(v["schema_version"], 1);
    assert_eq!(v["command"], "build");
    assert_eq!(v["ok"], false);
    let diags = v["diagnostics"].as_array().expect("diagnostics array");
    assert!(!diags.is_empty(), "forwarded check diagnostics expected");
    assert_eq!(diags[0]["code"], "E0202");
    assert_eq!(diags[0]["severity"], "error");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn nula_test_json_reports_per_test_results() {
    let dir = temp_dir("testjson");
    scaffold_package(
        &dir,
        "fn main() = 1\n",
        &[
            ("test_pass.nula", "fn main() = 1\n"),
            ("test_fail.nula", "fn main() = countr + 1\n"),
        ],
    );

    let out = run_nula(&dir, &["test", "--json"]);
    assert!(!out.status.success(), "one failing test → nonzero exit");
    let stdout = String::from_utf8(out.stdout).unwrap();
    let trimmed = stdout.trim();
    assert!(
        trimmed.starts_with('{') && trimmed.ends_with('}'),
        "stdout JSON only: {stdout:?}"
    );
    let v: serde_json::Value = serde_json::from_str(trimmed)
        .unwrap_or_else(|e| panic!("stdout must parse as JSON: {e}\n{stdout:?}"));
    assert_eq!(v["schema_version"], 1);
    assert_eq!(v["command"], "test");
    assert_eq!(v["ok"], false);

    let tests = v["tests"].as_array().expect("tests array");
    assert_eq!(tests.len(), 2, "one result per test: {tests:?}");
    for t in tests {
        assert!(t["name"].is_string());
        assert!(t["duration_ms"].is_number());
        let status = t["status"].as_str().unwrap();
        assert!(status == "ok" || status == "failed");
        if status == "failed" {
            assert!(
                !t["diagnostics"].as_array().unwrap().is_empty(),
                "failed test must carry diagnostics"
            );
        }
    }
    let statuses: Vec<&str> = tests
        .iter()
        .map(|t| t["status"].as_str().unwrap())
        .collect();
    assert!(statuses.contains(&"ok"));
    assert!(statuses.contains(&"failed"));
    // No human test output may leak into stdout.
    assert!(!stdout.contains("test result:"));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn nula_test_json_all_passing_is_ok() {
    let dir = temp_dir("testpass");
    scaffold_package(
        &dir,
        "fn main() = 1\n",
        &[("test_a.nula", "fn main() = 1\n")],
    );

    let out = run_nula(&dir, &["test", "--json"]);
    assert!(
        out.status.success(),
        "passing tests → exit 0: {:?}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    let v: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("stdout must parse as JSON: {e}\n{stdout:?}"));
    assert_eq!(v["ok"], true);
    let tests = v["tests"].as_array().expect("tests array");
    assert_eq!(tests.len(), 1);
    assert_eq!(tests[0]["status"], "ok");

    let _ = std::fs::remove_dir_all(&dir);
}
