# Apple `nulang-ui/1` host

Status: Experimental protocol host.

The Apple UI host mirrors the frozen Rust `nulang-ui/1` / `nulang-ui-msg/1` contract in pure Swift, maintains validated document state, and renders a deliberately small semantic primitive set through SwiftUI.

## Architecture

```text
Nulang runtime callback JSON
        |
        v
NulangUIProtocol (Foundation)
  - lossless WireValue decoding
  - protocol/version validation
  - tree invariants
  - ordered atomic patches
  - action envelope encoding
        |
        v
NulangUIStore (@MainActor)
  - publishes only validated state
  - preserves previous document on invalid patches
  - emits invoke_action messages
        |
        v
NulangUIRootView (SwiftUI)
  - column / row
  - text / button
  - spacer / divider
```

The protocol/state layer intentionally has no dependency on SwiftUI. This keeps the frozen ABI independently testable and gives future UIKit, AppKit, or test renderers the same state machine.

## Protocol fidelity

The Swift model follows the Rust protocol rather than normalizing it into convenient-but-lossy native JSON values:

- `revision` is a decimal string carrying the full `UInt64` domain.
- `i64` values are decimal strings carrying the full `Int64` domain.
- `f64` values are exact 16-hex-digit IEEE-754 bit payloads.
- byte values remain byte arrays.
- node IDs, action IDs, correlation IDs, and idempotency keys are validated as non-empty strings.
- semantic trees reject duplicate IDs, dangling/duplicate children, multiple parents, cycles, and unreachable nodes.
- patches verify protocol, document ID, base revision, and strictly increasing revision.
- patch application is atomic: invalid operations never publish a partial document.

## Concurrency boundary

`NulangUIStore` is `@MainActor`. Runtime/XCFramework work must remain on a dedicated worker; native callbacks should copy their JSON synchronously and then hop to the main actor before calling `applyRuntimeJSON`.

The store never runs server actions itself. Button interaction emits the frozen `invoke_action` message with the binding's `client`/`server` placement, current document revision, a correlation ID, and a separate idempotency key. The layer above the store decides whether that envelope returns to the local runtime or crosses the explicit server/network policy boundary.

## Primitive scope

The first renderer intentionally supports only:

- `column`
- `row`
- `text`
- `button`
- `spacer`
- `divider`

Unknown kinds render an explicit unsupported-node diagnostic. This is preferable to silently guessing semantics and follows the universal-app milestone's rule to prove host correctness before expanding the UI surface.

## Validation

Run the host package on macOS:

```sh
swift test --package-path platforms/ios/NulangUIHost
```

CI also compiles the package for a generic iOS Simulator destination:

```sh
cd platforms/ios/NulangUIHost
xcodebuild \
  -scheme NulangUIHost \
  -destination 'generic/platform=iOS Simulator' \
  CODE_SIGNING_ALLOWED=NO \
  build
```

`.github/workflows/apple-ui-host.yml` is the exact-head merge gate for this layer.

## Integration with the native runtime

This package is deliberately parallel to `NulangMobile.xcframework` packaging. Once both branches are green, the integration wrapper should:

1. create `NulangMobileApp` off the main thread;
2. copy `nulang_ui_document` / `nulang_ui_message` callback strings before the C callback returns;
3. submit copied JSON to `NulangUIStore` on `MainActor`;
4. encode emitted host action messages with `NulangUICodec`;
5. route client actions back into the runtime and server actions into the configured network/durable policy.

Keeping protocol rendering separate from runtime packaging lets failures be attributed to the correct boundary and prevents Swift from duplicating unsafe Rust/C lifecycle logic.
