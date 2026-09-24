//! End-to-end contracts for semantic source queries.

use std::path::{Path, PathBuf};
use std::process::Command;

fn nulang_exe() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_nulang"))
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "nulang_semantic_query_{}_{}_{}",
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
        "fn add(a: Int, b: Int) -> Int { a + b }\n\
         fn twice(x: Int) -> Int { add(x, x) }\n\
         fn shadow(add: Int) -> Int { add + 1 }\n\
         fn main() -> Int { twice(2) + add(1, 2) }\n",
    )
    .expect("write source");
    path
}

fn run_json(args: &[&str], path: &Path) -> serde_json::Value {
    let out = Command::new(nulang_exe())
        .arg("query")
        .args(args)
        .arg(path)
        .arg("--json")
        .output()
        .expect("run semantic query");
    assert!(
        out.status.success(),
        "semantic query failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("query output must be JSON")
}

#[test]
fn type_query_returns_inferred_function_facts() {
    let dir = temp_dir("type");
    let src = write_source(&dir);
    let v = run_json(&["type", "twice"], &src);

    assert_eq!(v["schema_version"], 1);
    assert_eq!(v["command"], "type");
    let symbols = v["symbols"].as_array().expect("symbols");
    assert_eq!(symbols.len(), 1);
    assert_eq!(symbols[0]["name"], "twice");
    assert!(symbols[0]["inferred_type"]
        .as_str()
        .expect("inferred type")
        .contains("Int"));
    assert!(symbols[0]["inferred_effects"].is_string());
    assert!(symbols[0]["inferred_capability"].is_string());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn references_ignore_shadowed_locals() {
    let dir = temp_dir("refs");
    let src = write_source(&dir);
    let v = run_json(&["references", "add"], &src);

    let refs = v["references"].as_array().expect("references");
    assert_eq!(refs.len(), 2, "only direct module-level uses should match");
    let owners = refs
        .iter()
        .map(|r| r["owner"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert!(owners.contains(&"twice"));
    assert!(owners.contains(&"main"));
    assert!(!owners.contains(&"shadow"));
    assert!(refs.iter().all(|r| r["kind"] == "call"));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn callers_and_callees_form_a_direct_call_graph() {
    let dir = temp_dir("graph");
    let src = write_source(&dir);

    let callers = run_json(&["callers", "add"], &src);
    let caller_refs = callers["references"].as_array().expect("caller refs");
    assert_eq!(caller_refs.len(), 2);

    let callees = run_json(&["callees", "main"], &src);
    let callee_refs = callees["references"].as_array().expect("callee refs");
    let targets = callee_refs
        .iter()
        .map(|r| r["target"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(targets.len(), 2);
    assert!(targets.contains(&"twice"));
    assert!(targets.contains(&"add"));

    let _ = std::fs::remove_dir_all(&dir);
}
