# NulangMobileRuntime

Low-level Swift ownership wrapper for `NulangMobile.xcframework`.

This source directory is copied into the generated `NulangMobileRuntimePackage` by `scripts/build_apple_mobile_runtime.sh`. The generated package includes the exact XCFramework built in the same release run and compiles against it as binary target `CNulangMobile`.

The wrapper deliberately owns only native runtime transport concerns:

- lifetime of `NulangMobileApp`;
- serialized access to `run`, authorized client-action invocation, and `free`;
- immediate copying of borrowed C callback and action-result strings;
- caller-selected callback dispatch.

Semantic protocol decoding belongs to `NulangUIProtocol`; SwiftUI/MainActor state belongs to `NulangUIHost`. `invokeAction` is not arbitrary function invocation: it routes only through the compiler-authorized action table embedded in the mobile `.nbc` artifact.
