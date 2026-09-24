//! End-to-end contract tests for the machine-readable `nulang query` surface.

use std::path::{Path, PathBuf};
use std::process::Command;

fn nulang_exe() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_nulang"))
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "nulang_code_query_{}_{}_{}",
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

fn write_source(dir: &Path) -> PathBuf {
    let path = dir.join("sample.nula");
    std::fs::write(
        &path,
        "type UserId = Int\nfn add(a: Int, b: Int) -> Int = a + b\nfn main() = add(1, 2)\n",
    )
    .expect("write source");
    path
}

#[test]
fn query_symbols_json_reports_stable_symbol_metadata() {
    let dir = temp_dir("symbols");
    let src = write_source(&dir);

    let out = Command::new(nulang_exe())
        .args(["query", "symbols"])
        .arg(&src)
        .arg("--json")
        .output()
        .expect("run query symbols");

    assert!(
        out.status.success(),
        "query symbols should succeed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).expect("stdout utf8");
    let v: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("query output must be JSON");

    assert_eq!(v["schema_version"], 1);
    assert_eq!(v["command"], "symbols");
    assert_eq!(v["ok"], true);

    let symbols = v["symbols"].as_array().expect("symbols array");
    let add = symbols
        .iter()
        .find(|s| s["name"] == "add")
        .expect("add symbol");
    assert_eq!(add["kind"], "function");
    assert_eq!(add["qualified_name"], "add");
    assert_eq!(add["signature"], "fn add(a: Int, b: Int) -> Int");
    assert!(add["span"]["start_byte"].is_number());
    assert!(add["span"]["end_byte"].is_number());
    assert_eq!(add["span"]["line"], 2);

    let alias = symbols
        .iter()
        .find(|s| s["name"] == "UserId")
        .expect("UserId symbol");
    assert_eq!(alias["kind"], "type_alias");
    assert_eq!(alias["signature"], "type UserId = Int");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn query_symbol_json_selects_one_exact_symbol() {
    let dir = temp_dir("symbol");
    let src = write_source(&dir);

    let out = Command::new(nulang_exe())
        .args(["query", "symbol", "add"])
        .arg(&src)
        .arg("--json")
        .output()
        .expect("run query symbol");

    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).expect("stdout utf8");
    let v: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("query output must be JSON");
    let symbols = v["symbols"].as_array().expect("symbols array");
    assert_eq!(symbols.len(), 1);
    assert_eq!(symbols[0]["name"], "add");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn query_symbols_name_filter_is_case_insensitive() {
    let dir = temp_dir("filter");
    let src = write_source(&dir);

    let out = Command::new(nulang_exe())
        .args(["query", "symbols"])
        .arg(&src)
        .args(["--name", "userid", "--json"])
        .output()
        .expect("run filtered query");

    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).expect("stdout utf8");
    let v: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("query output must be JSON");
    let symbols = v["symbols"].as_array().expect("symbols array");
    assert_eq!(symbols.len(), 1);
    assert_eq!(symbols[0]["name"], "UserId");

    let _ = std::fs::remove_dir_all(&dir);
}


#[test]
fn query_symbol_preserves_top_level_let_binding() {
    let dir = temp_dir("top_level_let");
    let src = dir.join("bindings.nula");
    std::fs::write(&src, "let answer = 42\n").expect("write source");

    let out = Command::new(nulang_exe())
        .args(["query", "symbol", "answer"])
        .arg(&src)
        .arg("--json")
        .output()
        .expect("run query symbol");

    assert!(
        out.status.success(),
        "top-level binding query should succeed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).expect("stdout utf8");
    let v: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("query output must be JSON");
    let symbols = v["symbols"].as_array().expect("symbols array");
    assert_eq!(symbols.len(), 1);
    assert_eq!(symbols[0]["name"], "answer");
    assert_eq!(symbols[0]["kind"], "binding");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn query_json_failure_stays_machine_readable() {
    let dir = temp_dir("missing_json");
    let missing = dir.join("missing.nula");

    let out = Command::new(nulang_exe())
        .args(["query", "symbols"])
        .arg(&missing)
        .arg("--json")
        .output()
        .expect("run missing-file query");

    assert!(!out.status.success(), "missing input must exit nonzero");
    let stdout = String::from_utf8(out.stdout).expect("stdout utf8");
    let v: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("failure output must still be JSON");
    assert_eq!(v["schema_version"], 1);
    assert_eq!(v["command"], "symbols");
    assert_eq!(v["ok"], false);
    assert!(v["error"].as_str().is_some_and(|message| message.contains("cannot read")));
    assert_eq!(v["symbols"].as_array().map(Vec::len), Some(0));

    let _ = std::fs::remove_dir_all(&dir);
}
