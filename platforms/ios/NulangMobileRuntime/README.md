# NulangMobileRuntime

Low-level Swift ownership wrapper for `NulangMobile.xcframework`.

This source directory is copied into the generated `NulangMobileRuntimePackage` by `scripts/build_apple_mobile_runtime.sh`. The generated package includes the exact XCFramework built in the same release run and compiles against it as binary target `CNulangMobile`.

The wrapper deliberately owns only native runtime transport concerns:

- lifetime of `NulangMobileApp`;
- serialized access to `run` and `free`;
- immediate copying of borrowed C callback strings;
- caller-selected callback dispatch.

Semantic protocol decoding belongs to `NulangUIProtocol`; SwiftUI/MainActor state belongs to `NulangUIHost`. Client actions are not exposed here until the compiler/runtime action allowlist ABI is present.
