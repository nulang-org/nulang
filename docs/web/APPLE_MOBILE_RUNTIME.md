# Apple mobile runtime packaging

Status: Experimental build/release tooling.

`NulangMobile.xcframework` is the Apple binary distribution boundary for Nulang's interpreter-only embedded runtime. It contains the Rust static runtime and the shared platform-neutral C mobile bootstrap behind one Clang module, `CNulangMobile`.

The build also emits `NulangMobileRuntimePackage.zip`, a local Swift package that wraps the XCFramework with a small ownership/transport layer. Applications therefore do not need to reproduce C lifetime and callback-copying code themselves.

## What is packaged

The build produces two XCFramework library variants:

- iOS device: `arm64`
- iOS Simulator: universal `arm64 + x86_64`

Each archive combines:

1. `libnulang.a`, cross-compiled with `--no-default-features --features mobile-runtime`;
2. `platforms/native/bridge/nulang_mobile_host.c`, compiled for the same Apple target;
3. the stable embedding and mobile-host headers;
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
- serializes create/run/free access around the C runtime;
- copies callback JSON into Swift-owned `Data` before the C callback returns;
- forwards document/message bytes onto a caller-selected `DispatchQueue`;
- exposes synchronous `run()` for a higher-level host to call from its dedicated runtime worker.

It does **not** decode `nulang-ui/1`, mutate SwiftUI state, execute server actions, or invent a client-action entrypoint. Those semantics stay above the runtime transport boundary.

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
2. a portable strict-C11 lifecycle/callback test for the shared C bootstrap;
3. Rust cross-compilation for device, arm64 simulator, and x86_64 simulator;
4. C bootstrap compilation against the real Apple SDK for every slice;
5. archive combination and simulator `lipo`;
6. `xcodebuild -create-xcframework`;
7. architecture inspection and module-map presence check;
8. XCFramework proof-artifact packaging and checksum output;
9. generated Swift-package assembly around the exact XCFramework;
10. a real generic-iOS-Simulator `xcodebuild` of `NulangMobileRuntime`;
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
   |- register nulang_ui_document(String)
   |- register nulang_ui_message(String)
   `- canonical nulang_load_nbc
   |
   v
nulang_mobile_app_run
   |- initial nulang-ui/1 callback
   `- incremental nulang-ui-msg/1 callbacks
```

The current pre-registered native-function registry is process-global, so v1 deliberately allows one active `NulangMobileApp` per process. Platform lifecycle wrappers must serialize create/run/free operations. Callback pointers are borrowed only for the duration of each C callback, so the Swift wrapper copies them synchronously before dispatching work to another queue.

## Scope boundary

This package stops at the runtime/transport boundary. SwiftUI rendering and `MainActor` UI state application belong in `NulangUIHost` above it.

Client-action ingress also remains intentionally outside this transport wrapper until the compiler/runtime freezes a dedicated authorized action ABI. A Swift method that simply invokes an arbitrary exported function by name would bypass compiler placement/authorization semantics and is not an acceptable substitute.
