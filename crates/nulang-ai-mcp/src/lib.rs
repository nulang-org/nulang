//! MCP (Model Context Protocol) JSON-RPC host and local coding tools.

pub mod dev_plane;
pub mod forge;
pub mod forge_tools;
pub mod gitea;
pub mod local_tools;
pub mod stdio;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

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

    /// Opt into MCP tool-level `isError` replies. The default preserves the
    /// existing forge gateway contract, which reports authorization failures
    /// as JSON-RPC errors for compatibility with current clients/tests.
    fn errors_as_tool_results(&self) -> bool {
        false
    }
}

pub struct ToolRegistry {
    tools: RwLock<HashMap<String, (ToolSpec, Arc<dyn ToolHandler>)>>,
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
        self.tools
            .read()
            .await
            .values()
            .map(|(s, _)| s.clone())
            .collect()
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
        let result: Result<Value, (i32, String)> = if req.jsonrpc != "2.0" {
            Err((-32600, "Invalid JSON-RPC version".to_string()))
        } else {
            match req.method.as_str() {
                "initialize" => Ok(serde_json::json!({
                    "protocolVersion": "2025-11-25",
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "nulang-ai-mcp", "version": env!("CARGO_PKG_VERSION")}
                })),
                "ping" => Ok(serde_json::json!({})),
                "tools/list" => Ok(self.tools_list().await),
                "tools/call" => self
                    .tools_call(req.params.unwrap_or_default())
                    .await
                    .map_err(|message| (-32602, message)),
                _ => Err((-32601, "Method not found".to_string())),
            }
        };
        match result {
            Ok(value) => JsonRpcResponse {
                jsonrpc: "2.0".into(),
                id,
                result: Some(value),
                error: None,
            },
            Err((code, message)) => JsonRpcResponse {
                jsonrpc: "2.0".into(),
                id,
                result: None,
                error: Some(JsonRpcError {
                    code,
                    message,
                    data: None,
                }),
            },
        }
    }

    async fn tools_list(&self) -> Value {
        let mut tools = self.registry.list_tools().await;
        tools.sort_by(|a, b| a.name.cmp(&b.name));
        let tools_json: Vec<Value> = tools
            .into_iter()
            .map(|t| serde_json::json!({
                "name": t.name,
                "description": t.description,
                "inputSchema": t.parameters
            }))
            .collect();
        serde_json::json!({"tools": tools_json})
    }

    async fn tools_call(&self, params: Value) -> Result<Value, String> {
        let name = params
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "Missing tool name".to_string())?;
        let args = params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));
        let handler = {
            let tools = self.registry.tools.read().await;
            Arc::clone(
                &tools
                    .get(name)
                    .ok_or_else(|| format!("Unknown tool: {name}"))?
                    .1,
            )
        };
        let errors_as_tool_results = handler.errors_as_tool_results();
        match handler.call(args).await {
            Ok(value) => Ok(serde_json::json!({
                "content": [{"type": "text", "text": value.to_string()}],
                "structuredContent": value,
                "isError": false
            })),
            Err(message) if errors_as_tool_results => Ok(serde_json::json!({
                "content": [{"type": "text", "text": message}],
                "isError": true
            })),
            Err(message) => Err(message),
        }
    }
}
