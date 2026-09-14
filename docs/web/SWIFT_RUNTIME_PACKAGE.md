# Swift runtime package boundary

Status: Experimental.

The generated `NulangMobileRuntimePackage` is the lowest Swift layer above the native XCFramework. It exists so application code does not need to manually own `NulangMobileApp *`, duplicate callback/result lifetime rules, or maintain a bridging header.

`NulangMobileRuntime` is intentionally synchronous and transport-only. A higher-level host owns the worker queue and calls `run()` or `invokeAction(requestJSON:)` there. Runtime callbacks are copied immediately into Swift-owned `Data`, then dispatched to the caller-selected callback queue. Action outputs are copied into Swift-owned `Data` while the serialized runtime lock is still held.

The wrapper does not decode `nulang-ui/1` or `nulang-ui-msg/1`; semantic decoding belongs to `NulangUIProtocol`. It does not publish SwiftUI state; that belongs to `NulangUIHost` on `MainActor`.

`invokeAction` is deliberately not a generic exported-function call. The native runtime accepts only canonical `nulang-ui-msg/1` `InvokeAction` requests whose opaque `action_id` is present in the compiler-generated mobile `.nbc` allowlist, executes the frozen `String -> String` reducer ABI in the interpreter, validates the canonical snapshot/patch output, and enforces document/revision causality before returning bytes to Swift.

## Ownership and concurrency

One `NulangMobileRuntime` owns one `NulangMobileApp`. All create/run/action/free access is serialized through the wrapper's lock, matching the native host's v1 single-app lifecycle contract. Higher-level code should still keep runtime work off `MainActor` and hop to the main actor only after protocol decoding produces UI state changes.

Borrowed native pointers never escape the wrapper:

- UI callback C strings are copied synchronously before the callback returns;
- action-output C strings are copied before the runtime lock is released;
- error buffers are copied into Swift errors before returning.
