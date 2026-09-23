---
title: "FS Effect"
description: "Built-in FS effect operations (auto-generated from src/stdlib.rs)"
sidebar:
  label: "FS"
editUrl: false
---

> **This page is auto-generated from `src/stdlib.rs`.**
> Do not edit it by hand — your changes will be overwritten on the next CI run.
> To add or update a built-in operation, edit the `StdLib::new()` registry in `src/stdlib.rs`.

# FS Effect

The `FS` effect provides the following built-in operations, wired into the VM and runtime.

| Operation | Signature | Description |
|-----------|-----------|-------------|
| `FS.read` | `read(path: String) -> String` | Read the entire contents of a file into a string; returns nil on error. |
| `FS.write` | `write(path: String, content: String) -> Unit` | Write a string to a file, overwriting any existing content; returns nil on error. |
| `FS.append` | `append(path: String, content: String) -> Unit` | Append a string to the end of a file, creating it if it does not exist; returns nil on error. |
| `FS.exists` | `exists(path: String) -> Bool` | Check whether a file or directory exists at the given path. |

_Implementation site: Standalone VM_
