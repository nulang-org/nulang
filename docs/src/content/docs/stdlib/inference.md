---
title: "Inference Effect"
description: "Built-in Inference effect operations (auto-generated from src/stdlib.rs)"
sidebar:
  label: "Inference"
editUrl: false
---

> **This page is auto-generated from `src/stdlib.rs`.**
> Do not edit it by hand — your changes will be overwritten on the next CI run.
> To add or update a built-in operation, edit the `StdLib::new()` registry in `src/stdlib.rs`.

# Inference Effect

The `Inference` effect provides the following built-in operations, wired into the VM and runtime.

| Operation | Signature | Description |
|-----------|-----------|-------------|
| `Inference.ask` | `ask(prompt: String) -> String` | Send the prompt to the configured inference provider and return the reply; suspends non-blockingly when the runtime supports it. |

_Implementation site: Runtime Host_
