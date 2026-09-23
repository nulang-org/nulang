---
title: "System Effect"
description: "Built-in System effect operations (auto-generated from src/stdlib.rs)"
sidebar:
  label: "System"
editUrl: false
---

> **This page is auto-generated from `src/stdlib.rs`.**
> Do not edit it by hand — your changes will be overwritten on the next CI run.
> To add or update a built-in operation, edit the `StdLib::new()` registry in `src/stdlib.rs`.

# System Effect

The `System` effect provides the following built-in operations, wired into the VM and runtime.

| Operation | Signature | Description |
|-----------|-----------|-------------|
| `System.arg` | `arg(n: Int) -> String` | Return the n-th command-line argument (0-based), or nil if out of range. Includes the program name at index 0. |

_Implementation site: Standalone VM_
