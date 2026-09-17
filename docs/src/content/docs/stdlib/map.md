---
title: "Map Effect"
description: "Built-in Map effect operations (auto-generated from src/stdlib.rs)"
sidebar:
  label: "Map"
editUrl: false
---

> **This page is auto-generated from `src/stdlib.rs`.**
> Do not edit it by hand — your changes will be overwritten on the next CI run.
> To add or update a built-in operation, edit the `StdLib::new()` registry in `src/stdlib.rs`.

# Map Effect

The `Map` effect provides the following built-in operations, wired into the VM and runtime.

| Operation | Signature | Description |
|-----------|-----------|-------------|
| `Map.new` | `new() -> Map` | Create an empty mutable hash map (open addressing). |
| `Map.insert` | `insert(m: Map, k: T, v: U) -> Map` | Insert key k with value v (overwrites existing); may grow and return a new pointer. |
| `Map.get` | `get(m: Map, k: T) -> U` | Look up key k; returns nil when absent. String keys compare by content. |
| `Map.contains` | `contains(m: Map, k: T) -> Bool` | True when key k is present. |
| `Map.remove` | `remove(m: Map, k: T) -> Map` | Remove key k if present (marks a tombstone). |
| `Map.size` | `size(m: Map) -> Int` | Return the number of live key-value pairs. |

_Implementation site: Standalone VM_
