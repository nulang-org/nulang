//! Standalone stdio MCP server exposing Nulang semantic compiler tools.

use nulang_ai_mcp::{
    semantic_mcp_server, JsonRpcError, JsonRpcRequest, JsonRpcResponse,
};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("nulang-mcp: {error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), String> {
    let server = semantic_mcp_server().await;
    let stdin = tokio::io::stdin();
    let mut lines = BufReader::new(stdin).lines();
    let mut stdout = tokio::io::stdout();

    while let Some(line) = lines
        .next_line()
        .await
        .map_err(|error| format!("failed to read stdin: {error}"))?
    {
        if line.trim().is_empty() {
            continue;
        }

        let request = match serde_json::from_str::<JsonRpcRequest>(&line) {
            Ok(request) => request,
            Err(error) => {
                write_response(
                    &mut stdout,
                    &JsonRpcResponse {
                        jsonrpc: "2.0".to_string(),
                        id: Some(Value::Null),
                        result: None,
                        error: Some(JsonRpcError {
                            code: -32700,
                            message: "Parse error".to_string(),
                            data: Some(Value::String(error.to_string())),
                        }),
                    },
                )
                .await?;
                continue;
            }
        };

        let notification = request.id.is_none();
        let response = server.handle_request(request).await;
        if !notification {
            write_response(&mut stdout, &response).await?;
        }
    }

    Ok(())
}

async fn write_response(
    stdout: &mut tokio::io::Stdout,
    response: &JsonRpcResponse,
) -> Result<(), String> {
    let mut bytes =
        serde_json::to_vec(response).map_err(|error| format!("serialize response: {error}"))?;
    bytes.push(b'\n');
    stdout
        .write_all(&bytes)
        .await
        .map_err(|error| format!("write response: {error}"))?;
    stdout
        .flush()
        .await
        .map_err(|error| format!("flush response: {error}"))
}
