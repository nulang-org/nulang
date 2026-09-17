---
title: "String Effect"
description: "Built-in String effect operations (auto-generated from src/stdlib.rs)"
sidebar:
  label: "String"
editUrl: false
---

> **This page is auto-generated from `src/stdlib.rs`.**
> Do not edit it by hand — your changes will be overwritten on the next CI run.
> To add or update a built-in operation, edit the `StdLib::new()` registry in `src/stdlib.rs`.

# String Effect

The `String` effect provides the following built-in operations, wired into the VM and runtime.

| Operation | Signature | Description |
|-----------|-----------|-------------|
| `String.to_int` | `to_int(value: String) -> Int` | Parse a string to an integer. Returns 0 for invalid input. |
| `String.to_float` | `to_float(value: String) -> Float` | Parse a string to a float. Returns 0.0 for invalid input. |
| `String.length` | `length(s: String) -> Int` | Return the length of the string in bytes. |
| `String.charAt` | `charAt(s: String, index: Int) -> Int` | Return the byte at the given index in the string, or -1 if out of bounds. |
| `String.from_char` | `from_char(code: Int) -> String` | Create a single-character string from a Unicode code point. Returns nil for invalid code points (surrogates, out of range). |
| `String.concat` | `concat(a: String, b: String) -> String` | Concatenate two strings. |
| `String.substring` | `substring(s: String, start: Int, len: Int) -> String` | Extract a substring. |

_Implementation site: Standalone VM_
