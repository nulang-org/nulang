# Native client-action protocol

Status: Experimental host/runtime integration on top of the frozen UI protocol.

Native client actions do **not** define another public wire vocabulary. The host/runtime boundary reuses the renderer-neutral protocol already frozen in `nulang-ui-protocol`:

- host → runtime: `nulang-ui-msg/1` `HostToRuntimeMessage::InvokeAction(ActionRequest)`
- runtime → host: `nulang-ui-msg/1` `RuntimeToHostMessage::{Snapshot, Patch}`

This keeps Swift, Kotlin, web, and future hosts on one action/message ABI.

## Invocation

A native host submits the same action request emitted by `NulangUIStore`:

```json
{
  "type": "invoke_action",
  "protocol": "nulang-ui-msg/1",
  "request": {
    "document_id": "document-1",
    "revision": "7",
    "action_id": "action_…",
    "placement": "client",
    "correlation_id": "corr-123",
    "idempotency_key": "idem-123",
    "payload": { "type": "null" }
  }
}
```

All identity/revision/value semantics come from `nulang-ui-protocol`; native code does not maintain a parallel schema.

`placement: "server"` is rejected by the local native action transport. Server actions belong to the explicit network/server policy layer and must never silently fall back to local execution.

## Output

An authorized local reducer returns one complete canonical runtime message as JSON: either a `snapshot` or `patch` envelope under `nulang-ui-msg/1`. The native runtime validates that message before returning it to the host.

There is intentionally no `nulang-action-result/1` wrapper. The native invocation is synchronous and the caller already owns the request/correlation context; adding another result envelope would duplicate the frozen UI message protocol without adding an authorization boundary.

## Authorization boundary

Wire validity is not execution authority. A later compiler/runtime layer must additionally require that the requested `action_id` appears in compiler-produced mobile artifact metadata. That allowlist is derived from semantic UI bindings and effect/type analysis, not from public exports, tools, or host-supplied function names.

Native-visible action IDs are deterministic opaque IDs derived from the compiled module identity and handler identity. Source handler names remain audit/compiler metadata only.

## Reducer ABI

The initial local execution primitive may use an internal `String -> String` reducer ABI:

1. input string: canonical serialized `HostToRuntimeMessage::InvokeAction`;
2. output string: canonical serialized `RuntimeToHostMessage`;
3. both directions are validated through `nulang-ui-protocol` before or after VM execution.

This reducer ABI is an internal artifact/runtime contract, not a second host protocol.
