---
title: "Signal Effect"
description: "Built-in Signal effect operations (auto-generated from src/stdlib.rs)"
sidebar:
  label: "Signal"
editUrl: false
---

> **This page is auto-generated from `src/stdlib.rs`.**
> Do not edit it by hand — your changes will be overwritten on the next CI run.
> To add or update a built-in operation, edit the `StdLib::new()` registry in `src/stdlib.rs`.

# Signal Effect

The `Signal` effect provides the following built-in operations, wired into the VM and runtime.

| Operation | Signature | Description |
|-----------|-----------|-------------|
| `Signal.wait` | `wait(name: String) -> Unit` | Suspend the workflow until the named signal arrives, then resume with unit. |

_Implementation site: Runtime Host_
