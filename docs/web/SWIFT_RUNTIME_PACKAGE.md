# Swift runtime package boundary

Status: Experimental.

The generated `NulangMobileRuntimePackage` is the lowest Swift layer above the native XCFramework. It exists so application code does not need to manually own `NulangMobileApp *`, duplicate callback lifetime rules, or maintain a bridging header.

`NulangMobileRuntime` is intentionally synchronous and transport-only. A higher-level host owns the worker queue and calls `run()` there. Runtime callbacks are copied immediately into Swift-owned `Data`, then dispatched to the caller-selected callback queue.

The wrapper does not decode `nulang-ui/1` or `nulang-ui-msg/1`; that belongs to `NulangUIProtocol`. It does not publish SwiftUI state; that belongs to `NulangUIHost` on `MainActor`. It also does not expose arbitrary exported-function invocation as a substitute for native client actions. Client actions require the separate compiler-authorized action allowlist ABI before they are added to this package.
