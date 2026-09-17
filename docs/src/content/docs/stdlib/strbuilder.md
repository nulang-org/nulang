---
title: "StrBuilder Effect"
description: "Built-in StrBuilder effect operations (auto-generated from src/stdlib.rs)"
sidebar:
  label: "StrBuilder"
editUrl: false
---

> **This page is auto-generated from `src/stdlib.rs`.**
> Do not edit it by hand — your changes will be overwritten on the next CI run.
> To add or update a built-in operation, edit the `StdLib::new()` registry in `src/stdlib.rs`.

# StrBuilder Effect

The `StrBuilder` effect provides the following built-in operations, wired into the VM and runtime.

| Operation | Signature | Description |
|-----------|-----------|-------------|
| `StrBuilder.new` | `new() -> StrBuilder` | Create an empty mutable string builder. |
| `StrBuilder.push` | `push(b: StrBuilder, s: String) -> StrBuilder` | Append s to the builder (amortized O(1) with capacity doubling); rebind the returned pointer. |
| `StrBuilder.to_string` | `to_string(b: StrBuilder) -> String` | Materialize the builder contents as an immutable string. |
| `StrBuilder.len` | `len(b: StrBuilder) -> Int` | Return the number of bytes currently in the builder. |
| `StrBuilder.reset` | `reset(b: StrBuilder) -> StrBuilder` | Clear the builder contents (capacity is retained). |

_Implementation site: Standalone VM_
