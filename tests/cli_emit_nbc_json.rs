//! Integration coverage for machine-readable bytecode emission.
//! Package builds rely on this command so they can compile and diagnose
//! with one validated frontend pass rather than a separate --check process.

use std::path::{Path, PathBuf};
use std::process::Command;

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "nulang_emit_nbc_json_{}_{}_{}",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("create test directory");
    dir
}

fn emit_json(source: &Path, output: &Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_nulang"))
        .args(["--json", "--emit-nbc", "--out"])
        .arg(output)
        .arg(source)
        .env(
            "NULANG_STDLIB",
            Path::new(env!("CARGO_MANIFEST_DIR")).join("src/stdlib"),
        )
        .output()
        .expect("run bytecode emitter")
}

#[test]
fn emit_nbc_json_success_reports_once_and_writes_artifact() {
    let dir = temp_dir("success");
    let source = dir.join("main.nula");
    let output = dir.join("main.nbc");
    std::fs::write(&source, "fn main() = 42\n").unwrap();

    let result = emit_json(&source, &output);
    assert!(
        result.status.success(),
        "emission failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    let stdout = String::from_utf8(result.stdout).unwrap();
    let report: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("exactly one JSON report");
    assert_eq!(report["schema_version"], 1);
    assert_eq!(report["command"], "build");
    assert_eq!(report["ok"], true);
    assert_eq!(report["diagnostics"].as_array().unwrap().len(), 0);
    assert!(output.is_file(), "bytecode artifact must be produced");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn emit_nbc_json_type_error_reports_structured_diagnostics() {
    let dir = temp_dir("typeerror");
    let source = dir.join("main.nula");
    let output = dir.join("main.nbc");
    std::fs::write(&source, "fn main() = countr + 1\n").unwrap();

    let result = emit_json(&source, &output);
    assert!(!result.status.success(), "invalid source must fail");
    let stdout = String::from_utf8(result.stdout).unwrap();
    let report: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("exactly one JSON report");
    assert_eq!(report["schema_version"], 1);
    assert_eq!(report["command"], "build");
    assert_eq!(report["ok"], false);
    let diagnostics = report["diagnostics"].as_array().unwrap();
    assert!(!diagnostics.is_empty(), "compiler errors must be reported");
    assert_eq!(diagnostics[0]["code"], "E0202");
    assert_eq!(
        diagnostics[0]["span"]["file"].as_str(),
        Some(source.to_str().expect("utf8 test source path")),
        "emitter diagnostics must retain the actual source filename"
    );
    assert!(!output.exists(), "failed validation must not emit bytecode");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn package_build_resolves_imports_relative_to_entry_source() {
    let dir = temp_dir("imports");
    let src_dir = dir.join("src");
    std::fs::create_dir_all(src_dir.join("subpkg")).unwrap();
    std::fs::write(
        dir.join("Nulang.toml"),
        "[package]\nname = \"single-pass-imports\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    std::fs::write(
        src_dir.join("main.nula"),
        "import subpkg::helper\nfn main() = helper_value()\n",
    )
    .unwrap();
    std::fs::write(
        src_dir.join("subpkg/helper.nula"),
        "fn helper_value() -> Int { 42 }\n",
    )
    .unwrap();

    // Relative imports must resolve identically in regular and JSON builds.
    for json in [false, true] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_nulang"));
        command.arg("nula").arg("build").current_dir(&dir).env(
            "NULANG_STDLIB",
            Path::new(env!("CARGO_MANIFEST_DIR")).join("src/stdlib"),
        );
        if json {
            command.arg("--json");
        }
        let output = command.output().expect("run package build");
        assert!(
            output.status.success(),
            "build json={json} failed: stdout={}; stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        if json {
            let report: serde_json::Value =
                serde_json::from_slice(&output.stdout).expect("JSON build report");
            assert_eq!(report["ok"], true);
        }
        assert!(
            dir.join(".nula/dist/single-pass-imports.nbc").is_file(),
            "relative import build must emit bytecode"
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}
