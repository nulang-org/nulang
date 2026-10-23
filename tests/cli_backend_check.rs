//! Regression: `--check --backend` must validate the selected compiler backend,
//! not merely report success after the shared frontend accepts the source.
use std::process::{Command, Output};

fn check(tag: &str, source: &str, backend: &str, json: bool) -> Output {
    let dir = std::env::temp_dir().join(format!(
        "nulang_backend_check_{tag}_{}_{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("main.nula");
    std::fs::write(&path, source).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_nulang"));
    command.current_dir(&dir);
    command.args(["--backend", backend, "--check"]).arg(&path);
    command.env(
        "NULANG_STDLIB",
        concat!(env!("CARGO_MANIFEST_DIR"), "/src/stdlib"),
    );
    if json {
        command.arg("--json");
    }
    let result = command.output().expect("run check");
    assert!(
        !dir.join("out.wasm").exists(),
        "backend checking must not write WASM artifacts"
    );
    assert!(
        !dir.join("out.cwasm").exists(),
        "backend checking must not write AOT artifacts"
    );
    std::fs::remove_dir_all(dir).unwrap();
    result
}

#[test]
fn unknown_check_backend_must_fail_closed() {
    let result = check("unknown", "1 + 2", "nonexistent-backend", true);
    assert!(
        !result.status.success(),
        "an unknown backend must not pass check"
    );
    let v: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap_or_else(|e| {
        panic!(
            "JSON diagnostics: {e}; status={:?}, stderr={}",
            result.status,
            String::from_utf8_lossy(&result.stderr)
        )
    });
    assert_eq!(v["ok"], false);
    assert!(v["diagnostics"][0]["message"]
        .as_str()
        .unwrap()
        .contains("unknown backend"));
}

#[test]
#[cfg(feature = "wasm-backend")]
fn wasm_check_rejects_durable_workflow_without_creating_artifact() {
    let source = "workflow Order {\n    step reserve { 1 }\n    step charge { 2 }\n}\n";
    let result = check("workflow", source, "wasm", true);
    assert!(
        !result.status.success(),
        "WASM cannot guarantee durable workflows"
    );
    let v: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap_or_else(|e| {
        panic!(
            "JSON diagnostics: {e}; status={:?}, stderr={}",
            result.status,
            String::from_utf8_lossy(&result.stderr)
        )
    });
    assert_eq!(v["ok"], false);
    assert!(v["diagnostics"][0]["message"]
        .as_str()
        .unwrap()
        .contains("does not yet support durable workflow semantics"));
}

#[test]
#[cfg(feature = "wasm-backend")]
fn wasm_check_validates_pure_source_without_executing_or_writing() {
    let result = check("wasm-pure", "1 + 2", "wasm", false);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(String::from_utf8_lossy(&result.stdout).contains("Backend check passed (wasm)."));
}

#[test]
#[cfg(feature = "native-codegen")]
fn native_check_compiles_pure_source_without_running_it() {
    let result = check("native-pure", "1 + 2", "native", false);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(String::from_utf8_lossy(&result.stdout).contains("Backend check passed (native)."));
}

#[test]
#[cfg(not(feature = "wasm-backend"))]
fn unavailable_wasm_backend_does_not_pass_check() {
    let result = check("missing-wasm", "1 + 2", "wasm", true);
    assert!(!result.status.success(), "missing backend must fail closed");
    let v: serde_json::Value = serde_json::from_slice(&result.stdout).expect("JSON report");
    assert!(v["diagnostics"][0]["message"]
        .as_str()
        .unwrap()
        .contains("wasm backend not compiled in"));
}
