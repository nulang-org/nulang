---
title: "Env Effect"
description: "Built-in Env effect operations (auto-generated from src/stdlib.rs)"
sidebar:
  label: "Env"
editUrl: false
---

> **This page is auto-generated from `src/stdlib.rs`.**
> Do not edit it by hand — your changes will be overwritten on the next CI run.
> To add or update a built-in operation, edit the `StdLib::new()` registry in `src/stdlib.rs`.

# Env Effect

The `Env` effect provides the following built-in operations, wired into the VM and runtime.

| Operation | Signature | Description |
|-----------|-----------|-------------|
| `Env.get` | `get(name: String) -> String` | Return the value of the named environment variable, or nil if not set. |

_Implementation site: Standalone VM_
