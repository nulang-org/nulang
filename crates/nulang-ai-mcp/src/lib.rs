//! Model Context Protocol host and Nulang semantic-tool adapter.
//!
//! The server supports both MCP lifecycle eras:
//! - 2025-era clients use `initialize`.
//! - 2026-07-28 clients use `server/discover` and per-request `_meta`.
//!
//! Semantic tools call the compiler's core query API directly. They do not
//! depend on the optional Nulang AI runtime and they are intentionally
//! read-only; source mutation remains behind the explicit `nulang fix --safe`
//! command.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::RwLock;

pub const MODERN_PROTOCOL_VERSION: &str = "2026-07-28";
pub const LEGACY_PROTOCOL_VERSION: &str = "2025-11-25";
const SERVER_NAME: &str = "nulang-semantic-tools";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub id: Option<Value>,
    pub method: String,
    pub params: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    pub id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

#[async_trait]
pub trait ToolHandler: Send + Sync {
    async fn call(&self, args: Value) -> Result<Value, String>;
}

pub struct ToolRegistry {
    tools: RwLock<HashMap<String, (ToolSpec, Arc<dyn ToolHandler>)>>,
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: RwLock::new(HashMap::new()),
        }
    }

    pub async fn register(&self, spec: ToolSpec, handler: Arc<dyn ToolHandler>) {
        self.tools
            .write()
            .await
            .insert(spec.name.clone(), (spec, handler));
    }

    pub async fn list_tools(&self) -> Vec<ToolSpec> {
        let mut tools = self
            .tools
            .read()
            .await
            .values()
            .map(|(spec, _)| spec.clone())
            .collect::<Vec<_>>();
        tools.sort_by(|a, b| a.name.cmp(&b.name));
        tools
    }
}

pub struct McpServer {
    registry: Arc<ToolRegistry>,
}

impl McpServer {
    pub fn new(registry: Arc<ToolRegistry>) -> Self {
        Self { registry }
    }

    pub async fn handle_request(&self, req: JsonRpcRequest) -> JsonRpcResponse {
        let id = req.id.clone();
        if req.jsonrpc != "2.0" {
            return error_response(
                id,
                -32600,
                "Invalid Request",
                Some(json!("jsonrpc must be 2.0")),
            );
        }

        let modern = is_modern_request(&req);
        let result = match req.method.as_str() {
            "server/discover" => Ok(self.server_discover()),
            "initialize" => Ok(self.initialize(req.params.as_ref())),
            "ping" if !modern => Ok(json!({})),
            "notifications/initialized" if !modern => Ok(Value::Null),
            "tools/list" => self.tools_list().await,
            "tools/call" => self.tools_call(req.params.unwrap_or_default()).await,
            _ => Err(JsonRpcError {
                code: -32601,
                message: "Method not found".to_string(),
                data: Some(json!(req.method)),
            }),
        };

        match result {
            Ok(mut value) => {
                if modern {
                    stamp_modern_result(&mut value);
                }
                JsonRpcResponse {
                    jsonrpc: "2.0".into(),
                    id,
                    result: Some(value),
                    error: None,
                }
            }
            Err(error) => JsonRpcResponse {
                jsonrpc: "2.0".into(),
                id,
                result: None,
                error: Some(error),
            },
        }
    }

    fn server_discover(&self) -> Value {
        json!({
            "supportedVersions": [MODERN_PROTOCOL_VERSION],
            "capabilities": {
                "tools": {
                    "listChanged": false
                }
            },
            "instructions": "Use the Nulang semantic tools to inspect source types, references, callers, callees, and compact semantic context.",
            "ttlMs": 300000,
            "cacheScope": "public"
        })
    }

    fn initialize(&self, params: Option<&Value>) -> Value {
        let _requested = params
            .and_then(|value| value.get("protocolVersion"))
            .and_then(Value::as_str);

        json!({
            "protocolVersion": LEGACY_PROTOCOL_VERSION,
            "capabilities": {
                "tools": {
                    "listChanged": false
                }
            },
            "serverInfo": server_info(),
            "instructions": "Use the Nulang semantic tools to inspect source without scraping compiler prose."
        })
    }

    async fn tools_list(&self) -> Result<Value, JsonRpcError> {
        let tools = self.registry.list_tools().await;
        let tools_json = tools
            .into_iter()
            .map(|tool| {
                json!({
                    "name": tool.name,
                    "description": tool.description,
                    "inputSchema": tool.parameters
                })
            })
            .collect::<Vec<_>>();
        Ok(json!({ "tools": tools_json }))
    }

    async fn tools_call(&self, params: Value) -> Result<Value, JsonRpcError> {
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| JsonRpcError {
                code: -32602,
                message: "Missing tool name".to_string(),
                data: None,
            })?;
        let args = params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));

        let handler = {
            let tools = self.registry.tools.read().await;
            let (_, handler) = tools.get(name).ok_or_else(|| JsonRpcError {
                code: -32602,
                message: format!("Unknown tool: {name}"),
                data: None,
            })?;
            Arc::clone(handler)
        };
        let result = handler.call(args).await.map_err(|message| JsonRpcError {
            code: -32000,
            message,
            data: None,
        })?;

        Ok(json!({
            "content": [
                {
                    "type": "text",
                    "text": result.to_string()
                }
            ],
            "structuredContent": result,
            "isError": false
        }))
    }
}

fn error_response(
    id: Option<Value>,
    code: i32,
    message: &str,
    data: Option<Value>,
) -> JsonRpcResponse {
    JsonRpcResponse {
        jsonrpc: "2.0".into(),
        id,
        result: None,
        error: Some(JsonRpcError {
            code,
            message: message.to_string(),
            data,
        }),
    }
}

fn is_modern_request(req: &JsonRpcRequest) -> bool {
    if req.method == "server/discover" {
        return true;
    }
    req.params
        .as_ref()
        .and_then(|params| params.get("_meta"))
        .and_then(|meta| meta.get("io.modelcontextprotocol/protocolVersion"))
        .and_then(Value::as_str)
        == Some(MODERN_PROTOCOL_VERSION)
}

fn stamp_modern_result(result: &mut Value) {
    let Some(object) = result.as_object_mut() else {
        return;
    };
    object
        .entry("resultType".to_string())
        .or_insert_with(|| Value::String("complete".to_string()));

    let meta = object
        .entry("_meta".to_string())
        .or_insert_with(|| json!({}));
    if let Some(meta) = meta.as_object_mut() {
        meta.entry("io.modelcontextprotocol/serverInfo".to_string())
            .or_insert_with(server_info);
    }
}

fn server_info() -> Value {
    json!({
        "name": SERVER_NAME,
        "version": SERVER_VERSION
    })
}

#[derive(Debug, Deserialize)]
struct SemanticToolArgs {
    file: String,
    name: String,
}

struct SemanticQueryTool {
    command: &'static str,
}

#[async_trait]
impl ToolHandler for SemanticQueryTool {
    async fn call(&self, args: Value) -> Result<Value, String> {
        let args: SemanticToolArgs =
            serde_json::from_value(args).map_err(|error| format!("invalid arguments: {error}"))?;
        let report =
            nulang::semantic_query::query_file(self.command, &args.name, Path::new(&args.file))
                .map_err(|error| error.to_string())?;
        serde_json::to_value(report).map_err(|error| error.to_string())
    }
}

/// Register the read-only Nulang compiler tools on an existing MCP registry.
pub async fn register_nulang_semantic_tools(registry: &Arc<ToolRegistry>) {
    for (name, command, description) in [
        (
            "nulang_type",
            "type",
            "Return the compiler-inferred type, effect row, and capability for a Nulang declaration.",
        ),
        (
            "nulang_references",
            "references",
            "Find lexical-scope-aware references to a Nulang top-level symbol.",
        ),
        (
            "nulang_callers",
            "callers",
            "Find direct callers of a Nulang top-level function or callable symbol.",
        ),
        (
            "nulang_callees",
            "callees",
            "Find direct callees from a Nulang declaration or behavior owner.",
        ),
        (
            "nulang_context",
            "context",
            "Return a compact semantic slice: inferred symbol facts plus inbound references and outbound direct calls.",
        ),
    ] {
        registry
            .register(
                semantic_tool_spec(name, description),
                Arc::new(SemanticQueryTool { command }),
            )
            .await;
    }
}

fn semantic_tool_spec(name: &str, description: &str) -> ToolSpec {
    ToolSpec {
        name: name.to_string(),
        description: description.to_string(),
        parameters: json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": {
                "file": {
                    "type": "string",
                    "description": "Path to the Nulang source file."
                },
                "name": {
                    "type": "string",
                    "description": "Symbol or owner name to query."
                }
            },
            "required": ["file", "name"],
            "additionalProperties": false
        }),
    }
}

/// Build an MCP server preloaded with the Nulang semantic compiler tools.
pub async fn semantic_mcp_server() -> McpServer {
    let registry = Arc::new(ToolRegistry::new());
    register_nulang_semantic_tools(&registry).await;
    McpServer::new(registry)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn request(id: i64, method: &str, params: Option<Value>) -> JsonRpcRequest {
        JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(json!(id)),
            method: method.to_string(),
            params,
        }
    }

    fn temp_source() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "nulang_mcp_semantic_{}_{}.nula",
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
        .unwrap();
        path
    }

    #[tokio::test]
    async fn discover_advertises_modern_era() {
        let server = semantic_mcp_server().await;
        let response = server
            .handle_request(request(1, "server/discover", Some(json!({}))))
            .await;
        let result = response.result.unwrap();
        assert_eq!(result["resultType"], "complete");
        assert!(result["supportedVersions"]
            .as_array()
            .unwrap()
            .contains(&json!(MODERN_PROTOCOL_VERSION)));
        assert_eq!(
            result["supportedVersions"].as_array().unwrap(),
            &[json!(MODERN_PROTOCOL_VERSION)]
        );
        assert_eq!(
            result["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
            SERVER_NAME
        );
    }

    #[tokio::test]
    async fn initialize_supports_latest_legacy_era() {
        let server = semantic_mcp_server().await;
        let response = server
            .handle_request(request(
                1,
                "initialize",
                Some(json!({ "protocolVersion": LEGACY_PROTOCOL_VERSION })),
            ))
            .await;
        let result = response.result.unwrap();
        assert_eq!(result["protocolVersion"], LEGACY_PROTOCOL_VERSION);
        assert_eq!(result["serverInfo"]["name"], SERVER_NAME);
        assert!(result.get("resultType").is_none());
    }

    #[tokio::test]
    async fn semantic_context_tool_returns_structured_compiler_data() {
        let path = temp_source();
        let server = semantic_mcp_server().await;
        let file = path.to_str().unwrap();

        let response = server
            .handle_request(request(
                2,
                "tools/call",
                Some(json!({
                    "name": "nulang_context",
                    "arguments": {
                        "file": file,
                        "name": "main"
                    },
                    "_meta": {
                        "io.modelcontextprotocol/protocolVersion": MODERN_PROTOCOL_VERSION
                    }
                })),
            ))
            .await;
        let result = response.result.unwrap();
        assert_eq!(result["resultType"], "complete");
        assert_eq!(
            result["structuredContent"]["symbols"][0]["name"],
            "main"
        );
        assert_eq!(
            result["structuredContent"]["references"][0]["target"],
            "add"
        );

        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn tools_list_is_deterministic() {
        let server = semantic_mcp_server().await;
        let response = server
            .handle_request(request(3, "tools/list", Some(json!({}))))
            .await;
        let result = response.result.unwrap();
        let names = result["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            vec![
                "nulang_callees",
                "nulang_callers",
                "nulang_context",
                "nulang_references",
                "nulang_type"
            ]
        );
    }
}
