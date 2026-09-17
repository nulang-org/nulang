---
title: "Otp Effect"
description: "Built-in Otp effect operations (auto-generated from src/stdlib.rs)"
sidebar:
  label: "Otp"
editUrl: false
---

> **This page is auto-generated from `src/stdlib.rs`.**
> Do not edit it by hand — your changes will be overwritten on the next CI run.
> To add or update a built-in operation, edit the `StdLib::new()` registry in `src/stdlib.rs`.

# Otp Effect

The `Otp` effect provides the following built-in operations, wired into the VM and runtime.

| Operation | Signature | Description |
|-----------|-----------|-------------|
| `Otp.create_supervisor` | `create_supervisor(name: String, strategy: Int) -> Int \| Nil` | Create an OTP supervisor actor and return its id; strategy is 0=one_for_one, 1=one_for_all, 2=rest_for_one, 3=simple_one_for_one (any other value yields nil). Nil no-op outside a runtime. |
| `Otp.supervise_child` | `supervise_child(sup: Int, child: Actor, policy: Int) -> Nil` | Place an existing actor under a supervisor; policy is 0=permanent, 1=temporary, 2=transient, 3=respawn_on_node_loss (RFC 0014, durable children only; any other value is a no-op). Unknown supervisor ids are nil no-ops. |
| `Otp.set_template` | `set_template(sup: Int, type_name: String) -> Nil` | Set the child template of a simple_one_for_one supervisor to the named actor type, resolved against the performing module's actor metadata. Unknown types or supervisor ids are nil no-ops. |
| `Otp.start_child` | `start_child(sup: Int) -> Actor \| Nil` | Spawn a fresh child from a simple_one_for_one supervisor's template and supervise it; returns the child actor ref, or nil when the supervisor is unknown, has no template, or is not simple_one_for_one. |
| `Otp.terminate_child` | `terminate_child(sup: Int, child: Actor) -> Nil` | Remove a child from supervision WITHOUT restarting it and exit it cleanly (Normal). Unknown supervisors or children are nil no-ops. |
| `Otp.child_count` | `child_count(sup: Int) -> Int \| Nil` | Return the number of currently supervised children, or nil for an unknown supervisor id. |

_Implementation site: Runtime Host_
