use async_trait::async_trait;
use nulang_ai_mcp::{JsonRpcRequest, McpServer, ToolHandler, ToolRegistry, ToolSpec};
use serde_json::{json, Value};
use std::sync::{Arc, Weak};
use std::time::Duration;

struct NoopHandler;

#[async_trait]
impl ToolHandler for NoopHandler {
    async fn call(&self, _args: Value) -> Result<Value, String> {
        Ok(json!({"ok": true}))
    }
}

struct RegisterDuringCall {
    registry: Weak<ToolRegistry>,
}

#[async_trait]
impl ToolHandler for RegisterDuringCall {
    async fn call(&self, _args: Value) -> Result<Value, String> {
        let registry = self.registry.upgrade().expect("registry still alive");
        // Reentrancy must not deadlock: tool handlers may discover/register tools.
        registry.register(
            ToolSpec {
                name: "discovered".into(),
                description: "Registered during the parent tool call".into(),
                parameters: json!({"type":"object"}),
            },
            Arc::new(NoopHandler),
        ).await;
        Ok(json!({"registered": true}))
    }
}

#[tokio::test]
async fn tool_handler_can_register_another_tool_without_deadlock() {
    let registry = Arc::new(ToolRegistry::new());
    registry.register(
        ToolSpec {
            name: "register".into(),
            description: "Discover a tool".into(),
            parameters: json!({"type":"object"}),
        },
        Arc::new(RegisterDuringCall { registry: Arc::downgrade(&registry) }),
    ).await;

    let server = McpServer::new(Arc::clone(&registry));
    let response = tokio::time::timeout(
        Duration::from_secs(2),
        server.handle_request(JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(json!(1)),
            method: "tools/call".into(),
            params: Some(json!({"name":"register", "arguments": {}})),
        }),
    ).await.expect("tool call blocked registry mutation while awaiting handler");
    assert!(response.error.is_none(), "unexpected MCP error: {:?}", response.error);
    assert_eq!(registry.list_tools().await.len(), 2);
}
