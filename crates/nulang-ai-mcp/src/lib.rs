//! MCP (Model Context Protocol) JSON-RPC host — OSS stub.

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
        let result = match req.method.as_str() {
            "tools/list" => self.tools_list().await,
            "tools/call" => self.tools_call(req.params.unwrap_or_default()).await,
            _ => Err("Method not found".to_string()),
        };
        match result {
            Ok(value) => JsonRpcResponse {
                jsonrpc: "2.0".into(),
                id,
                result: Some(value),
                error: None,
            },
            Err(message) => JsonRpcResponse {
                jsonrpc: "2.0".into(),
                id,
                result: None,
                error: Some(JsonRpcError {
                    code: -32603,
                    message,
                    data: None,
                }),
            },
        }
    }

    async fn tools_list(&self) -> Result<Value, String> {
        let tools = self.registry.list_tools().await;
        let tools_json: Vec<Value> = tools
            .into_iter()
            .map(|t| serde_json::json!({"name": t.name, "description": t.description, "inputSchema": t.parameters}))
            .collect();
        Ok(serde_json::json!({"tools": tools_json}))
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
        let tools = self.registry.tools.read().await;
        let (_, handler) = tools
            .get(name)
            .ok_or_else(|| format!("Unknown tool: {name}"))?;
        let result = handler.call(args).await?;
        Ok(serde_json::json!({"content": [{"type": "text", "text": result.to_string()}]}))
    }
}
