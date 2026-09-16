---
title: "Test Effect"
description: "Built-in Test effect operations (auto-generated from src/stdlib.rs)"
sidebar:
  label: "Test"
editUrl: false
---

> **This page is auto-generated from `src/stdlib.rs`.**
> Do not edit it by hand — your changes will be overwritten on the next CI run.
> To add or update a built-in operation, edit the `StdLib::new()` registry in `src/stdlib.rs`.

# Test Effect

The `Test` effect provides the following built-in operations, wired into the VM and runtime.

| Operation | Signature | Description |
|-----------|-----------|-------------|
| `Test.assert` | `assert(cond: Bool, msg: String) -> Unit ! {Test}` | Assert a condition is true; raises a runtime error with the given message on failure. |
| `Test.assert_eq` | `assert_eq(a: Int, b: Int) -> Unit ! {Test}` | Assert two integers are equal; raises a runtime error on failure. |
| `Test.assert_true` | `assert_true(cond: Bool) -> Unit ! {Test}` | Assert a condition is true; raises a runtime error on failure. |

_Implementation site: Standalone VM_
