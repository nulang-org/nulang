use nulang_ai_mcp::local_tools::{register_local_tools, LocalToolConfig};
use nulang_ai_mcp::stdio::serve_stream;
use nulang_ai_mcp::{McpServer, ToolRegistry};
use serde_json::{json, Value};
use std::io::Cursor;
use std::sync::Arc;

fn run_messages(lines: &[Value], workspace: &std::path::Path) -> Vec<Value> {
    run_messages_with_compiler(lines, workspace, std::path::Path::new("nulang"))
}

fn run_messages_with_compiler(
    lines: &[Value],
    workspace: &std::path::Path,
    compiler: &std::path::Path,
) -> Vec<Value> {
    let root = workspace.canonicalize().unwrap();
    let mut input = Vec::new();
    for line in lines {
        input.extend_from_slice(serde_json::to_string(line).unwrap().as_bytes());
        input.push(b'\n');
    }
    let registry = Arc::new(ToolRegistry::new());
    let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
    runtime.block_on(register_local_tools(
        &registry,
        LocalToolConfig::new(root, compiler.to_path_buf()),
    ));
    let server = McpServer::new(registry);
    let mut output = Vec::new();
    serve_stream(Cursor::new(input), &mut output, &server).unwrap();
    output
        .split(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).unwrap())
        .collect()
}

fn temp_workspace() -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!(
        "nulang-mcp-test-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&root).unwrap();
    root
}

#[test]
fn stdio_initializes_and_exposes_read_only_compiler_tool() {
    let root = temp_workspace();
    let replies = run_messages(
        &[
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"test","version":"1"}}}),
            json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
            json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
            json!({"jsonrpc":"2.0","id":3,"method":"ping"}),
        ],
        &root,
    );
    assert_eq!(replies.len(), 3, "notifications must not receive replies");
    assert_eq!(replies[0]["result"]["protocolVersion"], "2025-11-25");
    assert_eq!(replies[0]["result"]["capabilities"]["tools"], json!({}));
    let tools = replies[1]["result"]["tools"].as_array().unwrap();
    assert!(tools.iter().any(|tool| tool["name"] == "nulang_check"));
    assert_eq!(replies[2]["result"], json!({}));
}

#[test]
fn stdio_rejects_tools_before_initialize_and_reports_unknown_methods() {
    let root = temp_workspace();
    let replies = run_messages(
        &[
            json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
            json!({"jsonrpc":"2.0","id":2,"method":"initialize","params":{}}),
            json!({"jsonrpc":"2.0","id":3,"method":"does/not/exist"}),
        ],
        &root,
    );
    assert_eq!(replies[0]["error"]["code"], -32000);
    assert_eq!(replies[2]["error"]["code"], -32601);
}

#[test]
fn compiler_tool_rejects_workspace_escape_without_running_any_program() {
    let root = temp_workspace();
    std::fs::write(root.join("valid.nula"), "let x = 1\n").unwrap();
    let replies = run_messages(
        &[
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}),
            json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"nulang_check","arguments":{"path":"../outside.nula"}}}),
            json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"nulang_check","arguments":{"path":"/etc/passwd"}}}),
        ],
        &root,
    );
    for reply in &replies[1..] {
        assert_eq!(reply["result"]["isError"], true);
        assert!(reply["result"]["content"][0]["text"].is_string());
    }
}

#[cfg(unix)]
#[test]
fn compiler_tool_rejects_symlink_that_escapes_workspace() {
    use std::os::unix::fs::symlink;
    let root = temp_workspace();
    let outside = std::env::temp_dir().join(format!("nulang-mcp-outside-{}", std::process::id()));
    std::fs::write(&outside, "let x = 1\n").unwrap();
    let link = root.join("outside.nula");
    symlink(&outside, &link).unwrap();
    let replies = run_messages(
        &[
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}),
            json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"nulang_check","arguments":{"path":"outside.nula"}}}),
        ],
        &root,
    );
    assert_eq!(replies[1]["result"]["isError"], true);
    std::fs::remove_file(&link).unwrap();
    std::fs::remove_file(&outside).unwrap();
}

#[cfg(unix)]
#[test]
fn compiler_tool_roundtrip_returns_structured_diagnostics() {
    use std::os::unix::fs::PermissionsExt;
    let root = temp_workspace();
    std::fs::write(root.join("valid.nula"), "let x = 1\n").unwrap();
    let compiler = root.join("fake-nulang");
    std::fs::write(
        &compiler,
        "#!/bin/sh\nprintf '%s\\n' '{\"schema_version\":1,\"command\":\"check\",\"ok\":true,\"diagnostics\":[]}'\n",
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&compiler).unwrap().permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&compiler, permissions).unwrap();
    let replies = run_messages_with_compiler(
        &[
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}),
            json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"nulang_check","arguments":{"path":"valid.nula"}}}),
        ],
        &root,
        &compiler,
    );
    assert_eq!(replies[1]["result"]["isError"], false);
    assert_eq!(replies[1]["result"]["structuredContent"]["exit_success"], true);
    assert_eq!(replies[1]["result"]["structuredContent"]["report"]["ok"], true);
    assert_eq!(replies[1]["result"]["structuredContent"]["report"]["diagnostics"], json!([]));
}
