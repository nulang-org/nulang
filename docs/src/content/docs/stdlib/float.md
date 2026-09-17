---
title: "Float Effect"
description: "Built-in Float effect operations (auto-generated from src/stdlib.rs)"
sidebar:
  label: "Float"
editUrl: false
---

> **This page is auto-generated from `src/stdlib.rs`.**
> Do not edit it by hand — your changes will be overwritten on the next CI run.
> To add or update a built-in operation, edit the `StdLib::new()` registry in `src/stdlib.rs`.

# Float Effect

The `Float` effect provides the following built-in operations, wired into the VM and runtime.

| Operation | Signature | Description |
|-----------|-----------|-------------|
| `Float.to_int` | `to_int(value: Float) -> Int` | Convert a float to an integer by truncation toward zero. |
| `Float.to_string` | `to_string(value: Float) -> String` | Format a float as a string. |
| `Float.sin` | `sin(x: Float) -> Float` | Compute the sine of a float (radians). |
| `Float.cos` | `cos(x: Float) -> Float` | Compute the cosine of a float (radians). |
| `Float.sqrt` | `sqrt(x: Float) -> Float` | Compute the square root of a float. Returns nil for negative input. |
| `Float.tan` | `tan(x: Float) -> Float` | Compute the tangent of a float (radians). |
| `Float.log` | `log(x: Float) -> Float` | Compute the natural logarithm. Returns nil for x ≤ 0. |
| `Float.exp` | `exp(x: Float) -> Float` | Compute e to the power of x. |
| `Float.log2` | `log2(x: Float) -> Float` | Compute the base-2 logarithm. Returns nil for x ≤ 0. |
| `Float.log10` | `log10(x: Float) -> Float` | Compute the base-10 logarithm. Returns nil for x ≤ 0. |
| `Float.pow` | `pow(base: Float, exp: Float) -> Float` | Raise base to the exp power (base^exp). |

_Implementation site: Standalone VM_
