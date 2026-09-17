---
title: "Debug Effect"
description: "Built-in Debug effect operations (auto-generated from src/stdlib.rs)"
sidebar:
  label: "Debug"
editUrl: false
---

> **This page is auto-generated from `src/stdlib.rs`.**
> Do not edit it by hand — your changes will be overwritten on the next CI run.
> To add or update a built-in operation, edit the `StdLib::new()` registry in `src/stdlib.rs`.

# Debug Effect

The `Debug` effect provides the following built-in operations, wired into the VM and runtime.

| Operation | Signature | Description |
|-----------|-----------|-------------|
| `Debug.inspect` | `inspect(label: String, value: a) -> a` | Print a labeled value to stderr and return it unchanged. |

_Implementation site: Standalone VM_
