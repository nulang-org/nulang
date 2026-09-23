//! MCP (Model Context Protocol) JSON-RPC core for Nulang AI.
//!
//! This crate implements the transport-independent tool/discovery surface. The
//! HTTP/stdio transport layer is responsible for validating transport-specific
//! headers and carrying per-request metadata.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Latest MCP protocol revision implemented by this core dispatcher.
pub const MCP_PROTOCOL_VERSION: &str = "2026-07-28";

const JSON_RPC_INVALID_REQUEST: i32 = -32600;
const JSON_RPC_METHOD_NOT_FOUND: i32 = -32601;
const JSON_RPC_INVALID_PARAMS: i32 = -32602;
const MCP_UNSUPPORTED_PROTOCOL_VERSION: i32 = -32022;

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

impl JsonRpcResponse {
    fn success(id: Option<Value>, result: Value) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            result: Some(result),
            error: None,
        }
    }

    fn failure(id: Option<Value>, error: JsonRpcError) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            result: None,
            error: Some(error),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl JsonRpcError {
    fn invalid_request(message: impl Into<String>) -> Self {
        Self {
            code: JSON_RPC_INVALID_REQUEST,
            message: message.into(),
            data: None,
        }
    }

    fn method_not_found(method: &str) -> Self {
        Self {
            code: JSON_RPC_METHOD_NOT_FOUND,
            message: format!("Method not found: {method}"),
            data: None,
        }
    }

    fn invalid_params(message: impl Into<String>) -> Self {
        Self {
            code: JSON_RPC_INVALID_PARAMS,
            message: message.into(),
            data: None,
        }
    }

    fn unsupported_protocol_version(requested: &str) -> Self {
        Self {
            code: MCP_UNSUPPORTED_PROTOCOL_VERSION,
            message: "Unsupported protocol version".into(),
            data: Some(json!({
                "supported": [MCP_PROTOCOL_VERSION],
                "requested": requested,
            })),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// MCP `inputSchema`.
    pub parameters: Value,
}

#[async_trait]
pub trait ToolHandler: Send + Sync {
    /// Tool execution failures should return `Err`; the MCP server exposes
    /// those as a normal `CallToolResult` with `isError: true`.
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

    /// Return tools in deterministic name order so list responses are stable
    /// and safe for client/prompt caching.
    pub async fn list_tools(&self) -> Vec<ToolSpec> {
        let mut tools: Vec<_> = self
            .tools
            .read()
            .await
            .values()
            .map(|(spec, _)| spec.clone())
            .collect();
        tools.sort_by(|a, b| a.name.cmp(&b.name));
        tools
    }

    async fn handler(&self, name: &str) -> Option<Arc<dyn ToolHandler>> {
        self.tools
            .read()
            .await
            .get(name)
            .map(|(_, handler)| Arc::clone(handler))
    }
}

pub struct McpServer {
    registry: Arc<ToolRegistry>,
    server_name: String,
    server_version: String,
}

impl McpServer {
    pub fn new(registry: Arc<ToolRegistry>) -> Self {
        Self::with_identity(
            registry,
            env!("CARGO_PKG_NAME"),
            env!("CARGO_PKG_VERSION"),
        )
    }

    pub fn with_identity(
        registry: Arc<ToolRegistry>,
        name: impl Into<String>,
        version: impl Into<String>,
    ) -> Self {
        Self {
            registry,
            server_name: name.into(),
            server_version: version.into(),
        }
    }

    pub async fn handle_request(&self, req: JsonRpcRequest) -> JsonRpcResponse {
        let id = req.id.clone();

        if req.jsonrpc != "2.0" {
            return JsonRpcResponse::failure(
                id,
                JsonRpcError::invalid_request("jsonrpc must be exactly \"2.0\""),
            );
        }

        // Modern MCP carries the version in params._meta on every request.
        // The transport may serve older clients too, so absence is left to the
        // transport/lifecycle adapter; a present unsupported version is always
        // rejected deterministically.
        if let Some(requested) = protocol_version(&req.params) {
            if requested != MCP_PROTOCOL_VERSION {
                return JsonRpcResponse::failure(
                    id,
                    JsonRpcError::unsupported_protocol_version(requested),
                );
            }
        }

        let result = match req.method.as_str() {
            "server/discover" => self.server_discover(),
            "tools/list" => self.tools_list(req.params.as_ref()).await,
            "tools/call" => self.tools_call(req.params.as_ref()).await,
            _ => Err(JsonRpcError::method_not_found(&req.method)),
        };

        match result {
            Ok(value) => JsonRpcResponse::success(id, value),
            Err(error) => JsonRpcResponse::failure(id, error),
        }
    }

    fn server_discover(&self) -> Result<Value, JsonRpcError> {
        Ok(json!({
            "resultType": "complete",
            "supportedVersions": [MCP_PROTOCOL_VERSION],
            "capabilities": {
                "tools": {
                    "listChanged": false
                }
            },
            "_meta": {
                "io.modelcontextprotocol/serverInfo": {
                    "name": self.server_name,
                    "version": self.server_version,
                }
            },
            // Conservative defaults: immediately stale and private.
            "ttlMs": 0,
            "cacheScope": "private",
        }))
    }

    async fn tools_list(&self, params: Option<&Value>) -> Result<Value, JsonRpcError> {
        if let Some(cursor) = params
            .and_then(Value::as_object)
            .and_then(|p| p.get("cursor"))
            .filter(|cursor| !cursor.is_null())
        {
            return Err(JsonRpcError::invalid_params(format!(
                "Unsupported tools/list cursor: {cursor}"
            )));
        }

        let tools = self.registry.list_tools().await;
        let tools_json: Vec<Value> = tools
            .into_iter()
            .map(|tool| {
                json!({
                    "name": tool.name,
                    "description": tool.description,
                    "inputSchema": tool.parameters,
                })
            })
            .collect();

        Ok(json!({
            "resultType": "complete",
            "tools": tools_json,
            "ttlMs": 0,
            "cacheScope": "private",
        }))
    }

    async fn tools_call(&self, params: Option<&Value>) -> Result<Value, JsonRpcError> {
        let params = params
            .and_then(Value::as_object)
            .ok_or_else(|| JsonRpcError::invalid_params("tools/call params must be an object"))?;

        let name = params
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| JsonRpcError::invalid_params("Missing tool name"))?;

        let args = match params.get("arguments") {
            None | Some(Value::Null) => json!({}),
            Some(value) if value.is_object() => value.clone(),
            Some(_) => {
                return Err(JsonRpcError::invalid_params(
                    "tools/call arguments must be an object",
                ))
            }
        };

        // Clone the handler Arc before awaiting tool execution. Holding the
        // registry read guard across arbitrary async user code would block
        // registrations and can deadlock a handler that updates the registry.
        let handler = self
            .registry
            .handler(name)
            .await
            .ok_or_else(|| JsonRpcError::invalid_params(format!("Unknown tool: {name}")))?;

        match handler.call(args).await {
            Ok(result) => Ok(json!({
                "resultType": "complete",
                "content": [{
                    "type": "text",
                    "text": result.to_string()
                }],
                "structuredContent": result,
                "isError": false,
            })),
            Err(message) => Ok(json!({
                "resultType": "complete",
                "content": [{
                    "type": "text",
                    "text": message
                }],
                "isError": true,
            })),
        }
    }
}

fn protocol_version(params: &Option<Value>) -> Option<&str> {
    params
        .as_ref()?
        .get("_meta")?
        .get("io.modelcontextprotocol/protocolVersion")?
        .as_str()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Echo;

    #[async_trait]
    impl ToolHandler for Echo {
        async fn call(&self, args: Value) -> Result<Value, String> {
            Ok(args)
        }
    }

    struct Failing;

    #[async_trait]
    impl ToolHandler for Failing {
        async fn call(&self, _args: Value) -> Result<Value, String> {
            Err("provider temporarily unavailable".into())
        }
    }

    fn request(method: &str, params: Option<Value>) -> JsonRpcRequest {
        JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(json!(1)),
            method: method.into(),
            params,
        }
    }

    fn modern_params(extra: Value) -> Value {
        let mut object = extra.as_object().cloned().unwrap_or_default();
        object.insert(
            "_meta".into(),
            json!({
                "io.modelcontextprotocol/protocolVersion": MCP_PROTOCOL_VERSION,
                "io.modelcontextprotocol/clientCapabilities": {}
            }),
        );
        Value::Object(object)
    }

    #[tokio::test]
    async fn discover_advertises_modern_protocol_and_tools() {
        let server = McpServer::new(Arc::new(ToolRegistry::new()));
        let response = server
            .handle_request(request(
                "server/discover",
                Some(modern_params(json!({}))),
            ))
            .await;
        let result = response.result.expect("discover result");

        assert_eq!(result["resultType"], "complete");
        assert_eq!(result["supportedVersions"][0], MCP_PROTOCOL_VERSION);
        assert!(result["capabilities"]["tools"].is_object());
        assert_eq!(result["ttlMs"], 0);
        assert_eq!(result["cacheScope"], "private");
    }

    #[tokio::test]
    async fn tool_list_is_deterministic_and_cache_explicit() {
        let registry = Arc::new(ToolRegistry::new());
        registry
            .register(
                ToolSpec {
                    name: "zeta".into(),
                    description: "z".into(),
                    parameters: json!({"type": "object"}),
                },
                Arc::new(Echo),
            )
            .await;
        registry
            .register(
                ToolSpec {
                    name: "alpha".into(),
                    description: "a".into(),
                    parameters: json!({"type": "object"}),
                },
                Arc::new(Echo),
            )
            .await;

        let server = McpServer::new(registry);
        let response = server
            .handle_request(request(
                "tools/list",
                Some(modern_params(json!({}))),
            ))
            .await;
        let result = response.result.expect("tools/list result");

        assert_eq!(result["tools"][0]["name"], "alpha");
        assert_eq!(result["tools"][1]["name"], "zeta");
        assert_eq!(result["resultType"], "complete");
        assert_eq!(result["ttlMs"], 0);
        assert_eq!(result["cacheScope"], "private");
    }

    #[tokio::test]
    async fn unknown_tool_is_invalid_params_protocol_error() {
        let server = McpServer::new(Arc::new(ToolRegistry::new()));
        let response = server
            .handle_request(request(
                "tools/call",
                Some(modern_params(json!({
                    "name": "does_not_exist",
                    "arguments": {}
                }))),
            ))
            .await;

        let error = response.error.expect("protocol error");
        assert_eq!(error.code, JSON_RPC_INVALID_PARAMS);
        assert_eq!(error.message, "Unknown tool: does_not_exist");
    }

    #[tokio::test]
    async fn execution_failure_is_normal_tool_error_result() {
        let registry = Arc::new(ToolRegistry::new());
        registry
            .register(
                ToolSpec {
                    name: "unstable".into(),
                    description: "fails".into(),
                    parameters: json!({"type": "object"}),
                },
                Arc::new(Failing),
            )
            .await;

        let server = McpServer::new(registry);
        let response = server
            .handle_request(request(
                "tools/call",
                Some(modern_params(json!({
                    "name": "unstable",
                    "arguments": {}
                }))),
            ))
            .await;
        let result = response.result.expect("tool result");

        assert_eq!(result["resultType"], "complete");
        assert_eq!(result["isError"], true);
        assert_eq!(
            result["content"][0]["text"],
            "provider temporarily unavailable"
        );
    }

    #[tokio::test]
    async fn unsupported_modern_protocol_version_fails_with_supported_set() {
        let server = McpServer::new(Arc::new(ToolRegistry::new()));
        let response = server
            .handle_request(request(
                "tools/list",
                Some(json!({
                    "_meta": {
                        "io.modelcontextprotocol/protocolVersion": "1900-01-01",
                        "io.modelcontextprotocol/clientCapabilities": {}
                    }
                })),
            ))
            .await;

        let error = response.error.expect("unsupported version");
        assert_eq!(error.code, MCP_UNSUPPORTED_PROTOCOL_VERSION);
        assert_eq!(error.data.unwrap()["supported"][0], MCP_PROTOCOL_VERSION);
    }
}
