---
title: "Random Effect"
description: "Built-in Random effect operations (auto-generated from src/stdlib.rs)"
sidebar:
  label: "Random"
editUrl: false
---

> **This page is auto-generated from `src/stdlib.rs`.**
> Do not edit it by hand — your changes will be overwritten on the next CI run.
> To add or update a built-in operation, edit the `StdLib::new()` registry in `src/stdlib.rs`.

# Random Effect

The `Random` effect provides the following built-in operations, wired into the VM and runtime.

| Operation | Signature | Description |
|-----------|-----------|-------------|
| `Random.int` | `int(lo: Int, hi: Int) -> Int` | Return a random integer in the inclusive range [lo, hi]. |

_Implementation site: Standalone VM_
