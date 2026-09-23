---
title: "Realtime Effect"
description: "Built-in Realtime effect operations (auto-generated from src/stdlib.rs)"
sidebar:
  label: "Realtime"
editUrl: false
---

> **This page is auto-generated from `src/stdlib.rs`.**
> Do not edit it by hand — your changes will be overwritten on the next CI run.
> To add or update a built-in operation, edit the `StdLib::new()` registry in `src/stdlib.rs`.

# Realtime Effect

The `Realtime` effect provides the following built-in operations, wired into the VM and runtime.

| Operation | Signature | Description |
|-----------|-----------|-------------|
| `Realtime.broadcast` | `broadcast(room: String, message: String) -> Unit` | Broadcast a message to all subscribers of a realtime room. |

_Implementation site: Runtime Host_
