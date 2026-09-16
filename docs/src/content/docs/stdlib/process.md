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
| `Process.run` | `run(cmd: String) -> String` | Execute a shell command via /bin/sh -c and return its stdout; returns nil on error or non-zero exit. |

_Implementation site: Standalone VM_
