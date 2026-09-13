# Apple mobile runtime packaging

Status: Experimental build/release tooling.

`NulangMobile.xcframework` is the Apple binary distribution boundary for Nulang's interpreter-only embedded runtime. It contains the Rust static runtime and the shared platform-neutral C mobile bootstrap behind one Clang module, `CNulangMobile`.

The build also emits `NulangMobileRuntimePackage.zip`, a local Swift package that wraps the XCFramework with a small ownership/transport layer. Applications therefore do not need to reproduce C lifetime, callback copying, or action-result copying themselves.

## What is packaged

The build produces two XCFramework library variants:

- iOS device: `arm64`
- iOS Simulator: universal `arm64 + x86_64`

Each archive combines:

1. `libnulang.a`, cross-compiled with `--no-default-features --features mobile-runtime`;
2. `platforms/native/bridge/nulang_mobile_host.c`, compiled for the same Apple target;
3. the stable embedding, mobile-action, and mobile-host headers;
4. `CNulangMobile.h` plus `module.modulemap`, allowing Swift/Objective-C to `import CNulangMobile`.

No JIT/native-codegen dependency is permitted in this profile. Runtime construction inside the mobile bridge also uses `nulang_runtime_new_interpreter()` as a second line of enforcement.

## Generated Swift package

The same build creates:

```text
.nula/native/apple/NulangMobileRuntimePackage/
├── Package.swift
├── NulangMobile.xcframework/
└── Sources/
    └── NulangMobileRuntime/
        └── NulangMobileRuntime.swift
```

The `NulangMobileRuntime` target is intentionally low-level and UI-neutral. It:

- owns the opaque `NulangMobileApp` handle;
- serializes create/run/action/free access around the C runtime;
- copies callback JSON into Swift-owned `Data` before the C callback returns;
- copies borrowed action-result JSON into Swift-owned `Data` before unlocking;
- forwards document/message bytes onto a caller-selected `DispatchQueue`;
- exposes synchronous `run()` and `invokeAction(requestJSON:)` for a higher-level host to call from its dedicated runtime worker.

It does **not** decode `nulang-ui/1`, mutate SwiftUI state, or execute server actions. Client action invocation is not a generic exported-function escape hatch: the native runtime accepts only compiler-authorized action IDs frozen into the mobile `.nbc` allowlist.

## Build

On macOS with Xcode and Rust installed:

```sh
bash scripts/build_apple_mobile_runtime.sh
```

The default deployment target is iOS 15.0. Override it when necessary:

```sh
NULANG_IOS_MIN_VERSION=16.0 bash scripts/build_apple_mobile_runtime.sh
```

Artifacts are written under:

```text
.nula/native/apple/
  NulangMobile.xcframework/
  NulangMobile.xcframework.zip
  NulangMobileRuntimePackage/
  NulangMobileRuntimePackage.zip
```

The script prints SHA-256 values for both distributable zip files. Release automation should preserve those checksums alongside the artifacts.

## Validation sequence

The build script performs these gates in order:

1. `scripts/check_mobile_runtime_profile.sh` unless explicitly skipped by a parent CI job;
2. a portable strict-C11 lifecycle/callback/action test for the shared C bootstrap;
3. Rust cross-compilation for device, arm64 simulator, and x86_64 simulator;
4. C bootstrap compilation against the real Apple SDK for every slice;
5. archive combination and simulator `lipo`;
6. `xcodebuild -create-xcframework`;
7. architecture inspection and module-map presence check;
8. XCFramework proof-artifact packaging and checksum output;
9. generated Swift-package assembly around the exact XCFramework;
10. a real generic-iOS-Simulator `xcodebuild` of `NulangMobileRuntime`, including the authorized action method;
11. Swift-package proof-artifact packaging and checksum output.

`.github/workflows/apple-mobile-runtime.yml` runs the dependency/test gate on Linux first, then performs the real XCFramework and generated Swift-package build on `macos-latest`. A green workflow on the exact release commit is required before treating the Apple runtime package as distributable.

## Runtime lifecycle

Native applications should use the shared mobile host API rather than reproducing unsafe setup in Swift:

```text
app.nbc
   |
   v
NulangMobileRuntime (Swift owner)
   |
   v
nulang_mobile_app_new
   |- interpreter-only NulangRuntime
   |- restricted NulangMobileActionRuntime
   |- register nulang_ui_document(String)
   |- register nulang_ui_message(String)
   `- canonical nulang_load_nbc
   |
   +--> nulang_mobile_app_run
   |      |- initial nulang-ui/1 callback
   |      `- incremental nulang-ui-msg/1 callbacks
   |
   `--> nulang_mobile_app_invoke_action
          |- validate nulang-action-invoke/1
          |- resolve compiler allowlist entry
          |- run interpreter-only String -> String reducer
          `- validate correlated nulang-action-result/1
```

The current pre-registered native-function registry is process-global, so v1 deliberately allows one active `NulangMobileApp` per process. Platform lifecycle wrappers must serialize create/run/action/free operations. Callback pointers are borrowed only for the duration of each C callback, and action-result pointers are borrowed until the next action/free, so the Swift wrapper copies both synchronously at the boundary.

## Scope boundary

This package stops at the runtime/transport boundary. SwiftUI rendering and `MainActor` UI state application belong in `NulangUIHost` above it. `NulangMobileRuntime` transports already-validated client-action results but does not interpret their UI messages or decide server/network policy.
