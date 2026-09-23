---
title: "Python Effect"
description: "Built-in Python effect operations (auto-generated from src/stdlib.rs)"
sidebar:
  label: "Python"
editUrl: false
---

> **This page is auto-generated from `src/stdlib.rs`.**
> Do not edit it by hand — your changes will be overwritten on the next CI run.
> To add or update a built-in operation, edit the `StdLib::new()` registry in `src/stdlib.rs`.

# Python Effect

The `Python` effect provides the following built-in operations, wired into the VM and runtime.

| Operation | Signature | Description |
|-----------|-----------|-------------|
| `Python.import` | `import(module: String) -> Unit` | Import a Python module. |
| `Python.call` | `call(module: String, function: String, ...args) -> a` | Call a Python function with arguments. |
| `Python.get_attr` | `get_attr(module: String, attr: String) -> a` | Get an attribute from a Python module. |

_Implementation site: Runtime Host_
