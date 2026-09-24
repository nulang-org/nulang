//! End-to-end contract for `nulang query serve`.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

fn nulang_exe() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_nulang"))
}

fn temp_source() -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "nulang_query_serve_{}_{}.nula",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::write(
        &path,
        "fn add(a: Int, b: Int) -> Int { a + b }\nfn main() -> Int { add(1, 2) }\n",
    )
    .expect("write source");
    path
}

#[test]
fn query_serve_handles_persistent_jsonrpc_session() {
    let source = temp_source();
    let mut child = Command::new(nulang_exe())
        .args(["query", "serve"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn query daemon");

    let file = serde_json::to_string(source.to_str().unwrap()).unwrap();
    let input = format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\"}}\n\
         {{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"nulang/query/context\",\"params\":{{\"file\":{file},\"name\":\"main\"}}}}\n\
         {{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"shutdown\"}}\n"
    );
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(input.as_bytes())
        .expect("write requests");
    drop(child.stdin.take());

    let output = child.wait_with_output().expect("wait for query daemon");
    assert!(
        output.status.success(),
        "daemon failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let responses = String::from_utf8(output.stdout)
        .expect("stdout utf8")
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("json response"))
        .collect::<Vec<_>>();

    assert_eq!(responses.len(), 3);
    assert_eq!(responses[0]["result"]["schema_version"], 1);
    assert_eq!(responses[1]["result"]["symbols"][0]["name"], "main");
    assert_eq!(responses[1]["result"]["references"][0]["target"], "add");
    assert!(responses[2]["result"].is_null());

    let _ = std::fs::remove_file(source);
}
