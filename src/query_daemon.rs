//! Persistent JSON-RPC transport for semantic source queries.
//!
//! The daemon uses newline-delimited JSON-RPC 2.0 over stdio, matching the
//! transport shape used by agent protocols such as MCP while keeping the core
//! query engine independent of any specific agent runtime.

use crate::semantic_query::{
    analyze_source, query_index, SemanticIndex, SemanticQueryReport, SEMANTIC_QUERY_SCHEMA_VERSION,
};
use crate::types::{NuError, NuResult, Span};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

const JSONRPC_VERSION: &str = "2.0";

#[derive(Debug, Deserialize)]
struct RpcRequest {
    jsonrpc: String,
    #[serde(default)]
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct QueryParams {
    file: String,
    name: String,
}

#[derive(Debug)]
struct CachedIndex {
    digest: [u8; 32],
    index: SemanticIndex,
}

/// Persistent semantic-query session with content-addressed per-file caching.
///
/// Files are still read on each request so edits are observed immediately, but
/// parsing, import resolution, and typechecking are skipped when the exact
/// source bytes are unchanged.
#[derive(Debug, Default)]
pub struct QuerySession {
    cache: HashMap<PathBuf, CachedIndex>,
}

impl QuerySession {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn query(
        &mut self,
        command: &str,
        name: &str,
        path: &Path,
    ) -> NuResult<SemanticQueryReport> {
        let source = std::fs::read_to_string(path).map_err(|error| NuError::PackageError {
            msg: format!("cannot read '{}': {error}", path.display()),
            span: Span::default(),
        })?;
        let digest = *blake3::hash(source.as_bytes()).as_bytes();
        let path_buf = path.to_path_buf();

        let needs_refresh = self
            .cache
            .get(&path_buf)
            .map(|entry| entry.digest != digest)
            .unwrap_or(true);
        if needs_refresh {
            let index = analyze_source(path, &source)?;
            self.cache
                .insert(path_buf.clone(), CachedIndex { digest, index });
        }

        let index = self
            .cache
            .get(&path_buf)
            .ok_or_else(|| daemon_error("semantic query cache population failed".to_string()))?;
        query_index(command, name, &index.index)
    }

    pub fn invalidate(&mut self, path: &Path) -> bool {
        self.cache.remove(path).is_some()
    }

    pub fn clear(&mut self) {
        self.cache.clear();
    }

    pub fn cached_files(&self) -> usize {
        self.cache.len()
    }
}

pub fn run_stdio() -> NuResult<()> {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    run_io(stdin.lock(), stdout.lock())
}

/// Drive the query daemon over arbitrary line-oriented streams.
///
/// Every input line is one JSON-RPC 2.0 message and every response occupies one
/// output line. Notifications (requests without an id) intentionally produce
/// no response.
pub fn run_io<R: BufRead, W: Write>(mut reader: R, mut writer: W) -> NuResult<()> {
    let mut session = QuerySession::new();
    let mut line = String::new();

    loop {
        line.clear();
        let bytes = reader
            .read_line(&mut line)
            .map_err(|error| daemon_error(format!("failed to read query request: {error}")))?;
        if bytes == 0 {
            break;
        }
        if line.trim().is_empty() {
            continue;
        }

        let request = match serde_json::from_str::<RpcRequest>(line.trim()) {
            Ok(request) => request,
            Err(error) => {
                write_response(
                    &mut writer,
                    &rpc_error(Value::Null, -32700, "Parse error", Some(json!(error.to_string()))),
                )?;
                continue;
            }
        };

        let id = request.id.clone();
        let notification = id.is_none();
        let (response, shutdown) = handle_request(&mut session, request);

        if !notification {
            write_response(&mut writer, &response)?;
        }
        if shutdown {
            break;
        }
    }

    Ok(())
}

fn handle_request(session: &mut QuerySession, request: RpcRequest) -> (Value, bool) {
    let id = request.id.unwrap_or(Value::Null);
    if request.jsonrpc != JSONRPC_VERSION {
        return (
            rpc_error(id, -32600, "Invalid Request", Some(json!("jsonrpc must be 2.0"))),
            false,
        );
    }

    match request.method.as_str() {
        "initialize" => (
            rpc_success(
                id,
                json!({
                    "schema_version": SEMANTIC_QUERY_SCHEMA_VERSION,
                    "transport": "ndjson-jsonrpc-2.0",
                    "cache": "blake3-source",
                    "methods": [
                        "nulang/query/type",
                        "nulang/query/references",
                        "nulang/query/callers",
                        "nulang/query/callees",
                        "nulang/query/context",
                        "nulang/query/invalidate",
                        "nulang/query/clear"
                    ]
                }),
            ),
            false,
        ),
        "shutdown" => (rpc_success(id, Value::Null), true),
        "nulang/query/clear" => {
            session.clear();
            (rpc_success(id, json!({ "cleared": true })), false)
        }
        "nulang/query/invalidate" => {
            let Some(params) = request.params else {
                return (rpc_error(id, -32602, "Invalid params", None), false);
            };
            let Some(file) = params.get("file").and_then(Value::as_str) else {
                return (
                    rpc_error(id, -32602, "Invalid params", Some(json!("file is required"))),
                    false,
                );
            };
            let removed = session.invalidate(Path::new(file));
            (
                rpc_success(
                    id,
                    json!({ "invalidated": removed, "cached_files": session.cached_files() }),
                ),
                false,
            )
        }
        method if method.starts_with("nulang/query/") => {
            let command = &method["nulang/query/".len()..];
            if !matches!(
                command,
                "type" | "references" | "callers" | "callees" | "context"
            ) {
                return (rpc_error(id, -32601, "Method not found", None), false);
            }

            let params = match request
                .params
                .and_then(|value| serde_json::from_value::<QueryParams>(value).ok())
            {
                Some(params) => params,
                None => {
                    return (
                        rpc_error(
                            id,
                            -32602,
                            "Invalid params",
                            Some(json!("expected {file, name}")),
                        ),
                        false,
                    );
                }
            };
            match session.query(command, &params.name, Path::new(&params.file)) {
                Ok(report) => (rpc_success(id, json!(report)), false),
                Err(error) => (
                    rpc_error(id, -32000, "Query failed", Some(json!(error.to_string()))),
                    false,
                ),
            }
        }
        _ => (rpc_error(id, -32601, "Method not found", None), false),
    }
}

fn rpc_success(id: Value, result: Value) -> Value {
    json!({
        "jsonrpc": JSONRPC_VERSION,
        "id": id,
        "result": result,
    })
}

fn rpc_error(id: Value, code: i64, message: &str, data: Option<Value>) -> Value {
    let mut error = json!({
        "code": code,
        "message": message,
    });
    if let Some(data) = data {
        error["data"] = data;
    }
    json!({
        "jsonrpc": JSONRPC_VERSION,
        "id": id,
        "error": error,
    })
}

fn write_response<W: Write>(writer: &mut W, value: &Value) -> NuResult<()> {
    serde_json::to_writer(&mut *writer, value)
        .map_err(|error| daemon_error(format!("failed to serialize query response: {error}")))?;
    writer
        .write_all(b"\n")
        .and_then(|_| writer.flush())
        .map_err(|error| daemon_error(format!("failed to write query response: {error}")))
}

fn daemon_error(msg: String) -> NuError {
    NuError::PackageError {
        msg,
        span: Span::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn temp_file(tag: &str, source: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "nulang_query_daemon_{}_{}_{}.nula",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, source).expect("write temp source");
        path
    }

    #[test]
    fn session_refreshes_when_source_content_changes() {
        let path = temp_file("refresh", "fn answer() -> Int { 1 }\n");
        let mut session = QuerySession::new();

        let first = session.query("type", "answer", &path).expect("first query");
        assert_eq!(first.symbols.len(), 1);
        assert_eq!(session.cached_files(), 1);

        std::fs::write(&path, "fn answer() -> String { \"yes\" }\n").expect("rewrite source");
        let second = session.query("type", "answer", &path).expect("second query");
        assert!(second.symbols[0]
            .inferred_type
            .as_deref()
            .expect("type")
            .contains("String"));
        assert_eq!(session.cached_files(), 1);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn stdio_server_handles_multiple_requests_in_one_session() {
        let path = temp_file(
            "stdio",
            "fn add(a: Int, b: Int) -> Int { a + b }\nfn main() -> Int { add(1, 2) }\n",
        );
        let file = serde_json::to_string(path.to_str().unwrap()).unwrap();
        let input = format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\"}}\n\
             {{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"nulang/query/type\",\"params\":{{\"file\":{file},\"name\":\"add\"}}}}\n\
             {{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"nulang/query/callees\",\"params\":{{\"file\":{file},\"name\":\"main\"}}}}\n\
             {{\"jsonrpc\":\"2.0\",\"id\":4,\"method\":\"shutdown\"}}\n"
        );
        let mut output = Vec::new();
        run_io(Cursor::new(input), &mut output).expect("daemon");

        let responses = String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(responses.len(), 4);
        assert_eq!(responses[0]["result"]["schema_version"], 1);
        assert_eq!(responses[1]["result"]["symbols"][0]["name"], "add");
        assert_eq!(responses[2]["result"]["references"][0]["target"], "add");
        assert!(responses[3]["result"].is_null());

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn notification_produces_no_response() {
        let input = "{\"jsonrpc\":\"2.0\",\"method\":\"nulang/query/clear\"}\n";
        let mut output = Vec::new();
        run_io(Cursor::new(input), &mut output).expect("daemon");
        assert!(output.is_empty());
    }
}
