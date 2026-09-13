# Native client-action protocol

Status: Experimental wire-contract foundation.

Native client actions use a versioned request/result protocol rather than arbitrary exported-function invocation.

This layer defines the host-visible JSON contract only. It does **not** authorize any Nulang function for native invocation. Compiler/runtime authorization is a separate required layer.

## Request — `nulang-action-invoke/1`

```json
{
  "protocol": "nulang-action-invoke/1",
  "handler": "task.complete",
  "correlation_id": "c-123",
  "idempotency_key": "task:42:v7",
  "form": {
    "note": "done"
  },
  "signals": {
    "count": "3"
  }
}
```

`handler` and `correlation_id` are required and non-empty. `form` and `signals` are string maps in v1 so host state is explicit rather than hidden in VM globals. `idempotency_key` is carried through the protocol even though local client reducers may leave it empty; server boundaries can impose stricter requirements.

## Result — `nulang-action-result/1`

```json
{
  "protocol": "nulang-action-result/1",
  "correlation_id": "c-123",
  "messages": [
    {
      "protocol": "nulang-ui-msg/1",
      "message": {
        "type": "signal.set",
        "name": "count",
        "value": "4"
      }
    }
  ]
}
```

The result correlation ID must exactly match the request. Results reuse complete `nulang-ui-msg/1` envelopes; the action protocol does not introduce a second UI mutation vocabulary.

## Execution contract

The intended runtime semantics remain deliberately narrow:

1. the compiler discovers semantic UI actions classified for `client` placement;
2. only functions satisfying the frozen action ABI are recorded in a dedicated `.nbc` allowlist;
3. the native host submits a validated `nulang-action-invoke/1` snapshot;
4. the runtime resolves only the frozen allowlist entry;
5. the action runs as an isolated reducer invocation with the request JSON as its explicit input;
6. the returned string must decode as `nulang-action-result/1`, retain correlation, and contain valid `nulang-ui-msg/1` envelopes before the native host sees it.

A public/exported/tool function must not become a native action merely because its name matches the request.

## Compatibility

Within protocol major version 1, producers may add object fields that consumers can safely ignore. The following are major-version invariants:

- request/result protocol identifiers;
- explicit snapshot semantics for host form/signal state;
- correlation identity across an invocation;
- action results contain normal `nulang-ui-msg/1` envelopes;
- security-sensitive behavior is never enabled solely by an unknown field.

Changing client actions from isolated/snapshot semantics to hidden shared VM state requires a new protocol major.

## Next implementation slice

The next compiler/runtime PR must add a dedicated `CodeModule.client_actions` table with backward-compatible serialization, discover and validate bound client handlers at compile time, and make runtime invocation consult only that table. C, Swift, and JNI entry points come after that authorization primitive is in place.
