//! Line-delimited JSON-RPC 2.0 over stdio for legacy MCP clients.
//! Stdout is strictly reserved for protocol frames; diagnostics use stderr.

use crate::{JsonRpcError, JsonRpcRequest, JsonRpcResponse, McpServer};
use serde_json::Value;
use std::io::{self, BufRead, Write};

const MAX_MESSAGE_BYTES: usize = 1024 * 1024;

fn protocol_error(id: Option<Value>, code: i32, message: &str) -> JsonRpcResponse {
    JsonRpcResponse {
        jsonrpc: "2.0".into(),
        id,
        result: None,
        error: Some(JsonRpcError {
            code,
            message: message.into(),
            data: None,
        }),
    }
}

fn write_response<W: Write>(output: &mut W, response: &JsonRpcResponse) -> io::Result<()> {
    serde_json::to_writer(&mut *output, response)
        .map_err(io::Error::other)?;
    output.write_all(b"\n")?;
    output.flush()
}

/// Host lifetime is the lifetime of stdin. The transport accepts the legacy
/// 2025-11-25 MCP initialize handshake; it does not claim support for the
/// stateless 2026-07-28 MCP protocol revision.
pub fn serve_stream<R: BufRead, W: Write>(
    mut input: R,
    mut output: W,
    server: &McpServer,
) -> io::Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .map_err(io::Error::other)?;
    let mut initialized = false;
    let mut line = String::new();
    loop {
        line.clear();
        // Bound allocation per message, even if an untrusted caller omits '\n'.
        let bytes = input.by_ref().take((MAX_MESSAGE_BYTES + 1) as u64).read_line(&mut line)?;
        if bytes == 0 {
            return Ok(());
        }
        if bytes > MAX_MESSAGE_BYTES || !line.ends_with('\n') {
            // A truncated/oversized message cannot be safely resynchronized:
            // close the protocol stream rather than interpreting its tail.
            write_response(
                &mut output,
                &protocol_error(None, -32600, "MCP message too large or unterminated"),
            )?;
            return Ok(());
        }
        let message: Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(_) => {
                write_response(&mut output, &protocol_error(None, -32700, "Parse error"))?;
                continue;
            }
        };
        let request: JsonRpcRequest = match serde_json::from_value(message) {
            Ok(req) => req,
            Err(_) => {
                write_response(&mut output, &protocol_error(None, -32600, "Invalid request"))?;
                continue;
            }
        };
        // MCP notifications are one-way. Never emit responses to them.
        if request.id.is_none() {
            continue;
        }
        if !initialized && request.method != "initialize" {
            write_response(
                &mut output,
                &protocol_error(request.id, -32000, "MCP server not initialized"),
            )?;
            continue;
        }
        if request.method == "initialize" {
            initialized = true;
        }
        let response = runtime.block_on(server.handle_request(request));
        write_response(&mut output, &response)?;
    }
}
