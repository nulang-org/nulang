//! Real CLI boundary for read-only Web Contract IR preflight.
use std::process::Command;

#[test]
fn json_mode_returns_structured_failure_with_nonzero_exit() {
    let missing = std::env::temp_dir().join(format!(
        "nulang-web-preflight-cli-missing-{}",
        std::process::id()
    ));
    let output = Command::new(env!("CARGO_BIN_EXE_nulang"))
        .args(["web", "check", "--json"])
        .arg(&missing)
        .output()
        .expect("nulang binary must execute");
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stderr.is_empty());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["schema_version"], 1);
    assert_eq!(report["ok"], false);
    assert_eq!(report["route_count"], 0);
    assert_eq!(report["diagnostics"].as_array().unwrap().len(), 1);
}

#[test]
fn invalid_flag_is_a_usage_error_not_a_contract_failure() {
    let output = Command::new(env!("CARGO_BIN_EXE_nulang"))
        .args(["web", "check", "--oops"])
        .output()
        .expect("nulang binary must execute");
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("unknown web check option"));
}
