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

_Implementation site: Standalone VM_
