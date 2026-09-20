---
title: "Array Effect"
description: "Built-in Array effect operations (auto-generated from src/stdlib.rs)"
sidebar:
  label: "Array"
editUrl: false
---

> **This page is auto-generated from `src/stdlib.rs`.**
> Do not edit it by hand — your changes will be overwritten on the next CI run.
> To add or update a built-in operation, edit the `StdLib::new()` registry in `src/stdlib.rs`.

# Array Effect

The `Array` effect provides the following built-in operations, wired into the VM and runtime.

| Operation | Signature | Description |
|-----------|-----------|-------------|
| `Array.length` | `length(arr: [T]) -> Int` | Return the number of elements in the array. |
| `Array.push` | `push(arr: [T], elem: T) -> [T]` | Return a new array with elem appended to the end (value semantics). |
| `Array.new` | `new(n: Int, init: T) -> [T]` | Create a new array of n copies of init. |
| `Array.set` | `set(arr: [T], idx: Int, val: T) -> [T]` | Return a new array with index idx replaced by val (value semantics). |
| `Array.slice` | `slice(arr: [T], start: Int, end: Int) -> [T]` | Return a logical slice from start (inclusive) to end (exclusive); eligible slices use a zero-copy view internally. |
| `Array.compact` | `compact(arr: [T]) -> Bool` | Detach a zero-copy slice from an oversized backing array while preserving logical contents; ordinary arrays are unchanged. |

_Implementation site: Standalone VM_
