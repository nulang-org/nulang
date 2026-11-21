//! Local Nulang coding-agent MCP server.
//! Install with: cargo install --path crates/nulang-ai-mcp

use nulang_ai_mcp::local_tools::{register_local_tools, LocalToolConfig};
use nulang_ai_mcp::stdio::serve_stream;
use nulang_ai_mcp::{McpServer, ToolRegistry};
use std::path::PathBuf;
use std::sync::Arc;

fn main() {
    if let Err(error) = run() {
        eprintln!("nulang-ai-mcp: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let mut workspace = std::env::current_dir().map_err(|e| e.to_string())?;
    let mut compiler = PathBuf::from("nulang");
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--workspace" => {
                workspace = PathBuf::from(
                    args.next().ok_or("--workspace requires a directory")?
                );
            }
            "--compiler" => {
                compiler = PathBuf::from(
                    args.next().ok_or("--compiler requires a compiler executable")?
                );
            }
            "--help" | "-h" => {
                eprintln!("Usage: nulang-ai-mcp [--workspace DIR] [--compiler NULANG_BINARY]");
                eprintln!("Uses JSON-RPC over stdio; stdout is reserved for protocol responses.");
                return Ok(());
            }
            _ => return Err(format!("unknown option: {arg}")),
        }
    }
    let workspace = workspace
        .canonicalize()
        .map_err(|e| format!("cannot access workspace: {e}"))?;
    if !workspace.is_dir() {
        return Err("workspace must be an existing directory".into());
    }
    // Tool definitions and workspace limits are host-controlled and not
    // affected by user-supplied MCP tool call parameters.
    let registry = Arc::new(ToolRegistry::new());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .map_err(|e| e.to_string())?;
    runtime.block_on(register_local_tools(
        &registry,
        LocalToolConfig::new(workspace, compiler),
    ));
    let server = McpServer::new(registry);
    serve_stream(
        std::io::stdin().lock(),
        std::io::stdout().lock(),
        &server,
    )
    .map_err(|e| e.to_string())
}
