---
title: "Int Effect"
description: "Built-in Int effect operations (auto-generated from src/stdlib.rs)"
sidebar:
  label: "Int"
editUrl: false
---

> **This page is auto-generated from `src/stdlib.rs`.**
> Do not edit it by hand — your changes will be overwritten on the next CI run.
> To add or update a built-in operation, edit the `StdLib::new()` registry in `src/stdlib.rs`.

# Int Effect

The `Int` effect provides the following built-in operations, wired into the VM and runtime.

| Operation | Signature | Description |
|-----------|-----------|-------------|
| `Int.to_string` | `to_string(value: Int) -> String` | Convert an integer to its string representation. |
| `Int.to_float` | `to_float(value: Int) -> Float` | Convert an integer to a floating-point number. |
| `Int.to_hex` | `to_hex(n: Int) -> String` | Format an integer as a hexadecimal string (lowercase, no prefix). |
| `Int.to_binary` | `to_binary(n: Int) -> String` | Format an integer as a binary string. |

_Implementation site: Runtime Host_
