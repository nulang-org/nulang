# Nulang local coding-agent MCP server

`nulang-ai-mcp` is a **local stdio Model Context Protocol server** exposing
read-only compiler diagnostics to trusted coding-agent hosts. It does not
grant file-write, Git, network, or merge access.

This initial implementation supports the **2025-11-25 MCP handshake**. It does
not yet implement the stateless 2026-07-28 `server/discover` protocol.

## Install and run

From the repository root:

```sh
cargo install --locked --path crates/nulang-ai-mcp
# Install the Nulang CLI separately; it must be on PATH or explicitly provided.
nulang-ai-mcp --workspace /absolute/path/to/project --compiler nulang
```

The server reads newline-delimited JSON-RPC messages from stdin and writes
protocol responses to stdout. Do not send human-oriented logs to stdout.

The main CLI also forwards
`nulang agent mcp serve --workspace /absolute/path/to/project` to the
standalone host. Set `NULANG_MCP_BIN` to the host executable path if it is
not adjacent to `nulang` and not on PATH.

Example coding-agent configuration (adapt to the client):

```json
{
  "mcpServers": {
    "nulang": {
      "command": "nulang-ai-mcp",
      "args": [
        "--workspace", "/absolute/path/to/project",
        "--compiler", "nulang"
      ]
    }
  }
}
```

## Exposed tool

- `nulang_check({"path":"src/main.nula"})`: invoke `nulang --json --check`
  for one existing, workspace-relative `.nula` source file. The result
  contains `exit_success` and the complete versioned diagnostic `report`.
  Compiler errors are returned as ordinary structured results, not JSON-RPC
  transport errors. Tool invocation failures return `isError: true`.

Paths are canonicalized and must stay under the host-selected workspace.
Absolute paths, traversal, symlink escapes, and oversized input files are
rejected. The client cannot choose the compiler executable or arguments.

The source file and compiler must still be **trusted**. Canonical path checks
are not an operating-system sandbox: an existing project may import files
outside its root, and the compiler runs with the host account's privileges.
Use a restricted OS account or sandbox for untrusted repositories. The initial
implementation processes stdio requests serially and caps compiler execution at\n30 seconds, killing the tracked subprocess when the deadline expires. The host\ndoes not yet enforce full process-group cleanup or a strict memory limit.

The MCP stdio channel supports `initialize`, `notifications/initialized`,
`ping`, `tools/list`, and `tools/call`. Notification messages never
receive a response.

## Test gates

```sh
cargo test --locked -p nulang-ai-mcp
cargo fmt --all -- --check
cargo clippy --locked -p nulang-ai-mcp --all-targets -- -D warnings
```

See [../../docs/AGENT_DIAGNOSTICS.md](../../docs/AGENT_DIAGNOSTICS.md)
for the separate, not-yet-merged diagnostics schema extension proposed in
PR #1656. This server accepts the existing versioned JSON report without
depending on that PR.
