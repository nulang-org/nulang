---
title: "Process Effect"
description: "Built-in Process effect operations (auto-generated from src/stdlib.rs)"
sidebar:
  label: "Process"
editUrl: false
---

> **This page is auto-generated from `src/stdlib.rs`.**
> Do not edit it by hand — your changes will be overwritten on the next CI run.
> To add or update a built-in operation, edit the `StdLib::new()` registry in `src/stdlib.rs`.

# Process Effect

The `Process` effect provides the following built-in operations, wired into the VM and runtime.

| Operation | Signature | Description |
|-----------|-----------|-------------|
| `Process.run` | `run(cmd: String) -> String` | Trusted/standalone host primitive: execute a shell command via /bin/sh -c and return stdout; actor-backed runtimes do not dispatch it until an isolated process sandbox exists. |
| `Process.exec` | `exec(program: String, args: [String], timeout_ms: Int, max_output_bytes: Int) -> (Int, String, String)` | Trusted standalone-only direct argv execution (no implicit shell). Returns (exit_code, stdout, stderr), or nil for launch errors, timeouts, output-limit violations, or invalid arguments. UTF-8 output is lossily decoded. Bounds: timeout 1..60000ms, combined output 1..16777216 bytes. Actor-backed hosts fail closed. |
| `Process.exec_in` | `exec_in(program: String, args: [String], cwd: String, env: [(String, String)], timeout_ms: Int, max_output_bytes: Int) -> (Int, String, String)` | Trusted standalone argv execution with child-only working directory and environment overrides. Empty cwd inherits host cwd; other environment variables remain inherited. Returns (exit_code, stdout, stderr) or nil on failure. Bounds and actor isolation restrictions match Process.exec. |

_Implementation site: Standalone VM_
