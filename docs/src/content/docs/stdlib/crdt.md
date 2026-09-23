---
title: "Crdt Effect"
description: "Built-in Crdt effect operations (auto-generated from src/stdlib.rs)"
sidebar:
  label: "Crdt"
editUrl: false
---

> **This page is auto-generated from `src/stdlib.rs`.**
> Do not edit it by hand — your changes will be overwritten on the next CI run.
> To add or update a built-in operation, edit the `StdLib::new()` registry in `src/stdlib.rs`.

# Crdt Effect

The `Crdt` effect provides the following built-in operations, wired into the VM and runtime.

| Operation | Signature | Description |
|-----------|-----------|-------------|
| `Crdt.increment` | `increment(field: String) -> Unit` | Increment the current actor's CRDT-backed counter field (`gcounter`/`pncounter`). The op-set guard rejects other CRDT types (nil); nil also outside an actor. |
| `Crdt.decrement` | `decrement(field: String) -> Unit` | Decrement the current actor's `pncounter` field. Rejected (nil) on any other CRDT type; nil also outside an actor. |
| `Crdt.add` | `add(field: String, item: String) -> Unit` | Add an element to the current actor's CRDT set field (`gset`/`orset`/`aworset`). Rejected (nil) on any other CRDT type; nil also outside an actor. |
| `Crdt.remove` | `remove(field: String, item: String) -> Unit` | Remove an element from the current actor's CRDT set field (`orset`/`aworset`). Rejected (nil) on any other CRDT type; nil also outside an actor. |
| `Crdt.set` | `set(field: String, value: String) -> Unit` | Write a value to the current actor's CRDT register field (`lwwregister`/`mvregister`). Rejected (nil) on any other CRDT type; nil also outside an actor. |
| `Crdt.read` | `read(field: String) -> Int \| String` | Read the current actor's CRDT field materialized value: the count for counters, the element count for sets/RGA, and the stored string for `lwwregister` (nil on a missing field or outside an actor). |

_Implementation site: Runtime Host_
