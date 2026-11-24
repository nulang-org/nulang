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

#[test]
fn require_routes_rejects_empty_web_application_with_structured_diagnostics() {
    let root = std::env::temp_dir().join(format!(
        "nulang-web-preflight-empty-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/main.nula"), "fn main() { }").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_nulang"))
        .args(["web", "check", "--json", "--require-routes"])
        .arg(&root)
        .output()
        .expect("nulang binary must execute");
    let _ = std::fs::remove_dir_all(&root);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stderr.is_empty());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["schema_version"], 1);
    assert_eq!(report["route_count"], 0);
    assert_eq!(report["issues"][0]["code"], "WEB_ROUTES_REQUIRED");
    assert_eq!(report["issues"][0]["severity"], "error");
}
