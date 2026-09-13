#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT="${NULANG_APPLE_OUT:-$ROOT/.nula/native/apple}"
CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$OUT/cargo-target}"
IOS_MIN_VERSION="${NULANG_IOS_MIN_VERSION:-15.0}"

DEVICE_TARGET="aarch64-apple-ios"
SIM_ARM_TARGET="aarch64-apple-ios-sim"
SIM_X86_TARGET="x86_64-apple-ios"

export CARGO_TARGET_DIR

for tool in cargo rustup xcrun xcodebuild lipo libtool cc ditto shasum; do
    command -v "$tool" >/dev/null 2>&1 || {
        echo "missing required tool: $tool" >&2
        exit 1
    }
done

if [[ "${NULANG_SKIP_PROFILE_CHECK:-0}" != "1" ]]; then
    echo "==> Verifying interpreter-only mobile dependency profile"
    bash "$ROOT/scripts/check_mobile_runtime_profile.sh"
fi

echo "==> Running portable native bridge test"
mkdir -p "$OUT/test"
cc \
    -std=c11 \
    -Wall \
    -Wextra \
    -Werror \
    -I "$ROOT/include" \
    "$ROOT/platforms/native/bridge/nulang_mobile_host.c" \
    "$ROOT/platforms/native/bridge/tests/test_mobile_bridge.c" \
    -o "$OUT/test/test_mobile_bridge"
"$OUT/test/test_mobile_bridge"

echo "==> Installing Apple Rust targets"
rustup target add "$DEVICE_TARGET" "$SIM_ARM_TARGET" "$SIM_X86_TARGET"

rm -rf \
    "$OUT/device" \
    "$OUT/simulator-arm64" \
    "$OUT/simulator-x86_64" \
    "$OUT/simulator" \
    "$OUT/headers" \
    "$OUT/swift-derived-data" \
    "$OUT/NulangMobile.xcframework" \
    "$OUT/NulangMobile.xcframework.zip" \
    "$OUT/NulangMobileRuntimePackage" \
    "$OUT/NulangMobileRuntimePackage.zip"
mkdir -p \
    "$OUT/device" \
    "$OUT/simulator-arm64" \
    "$OUT/simulator-x86_64" \
    "$OUT/simulator" \
    "$OUT/headers"

build_rust() {
    local target="$1"
    echo "==> cargo build --target $target"
    cargo build \
        --manifest-path "$ROOT/Cargo.toml" \
        --release \
        --lib \
        --target "$target" \
        --no-default-features \
        --features mobile-runtime
}

build_bridge_object() {
    local sdk="$1"
    local clang_target="$2"
    local out="$3"
    local sdk_path

    sdk_path="$(xcrun --sdk "$sdk" --show-sdk-path)"
    xcrun --sdk "$sdk" clang \
        -std=c11 \
        -Wall \
        -Wextra \
        -Werror \
        -target "$clang_target" \
        -isysroot "$sdk_path" \
        -I "$ROOT/include" \
        -c "$ROOT/platforms/native/bridge/nulang_mobile_host.c" \
        -o "$out"
}

combine_static_library() {
    local rust_lib="$1"
    local bridge_obj="$2"
    local out="$3"

    /usr/bin/libtool -static \
        -o "$out" \
        "$rust_lib" \
        "$bridge_obj"
}

build_rust "$DEVICE_TARGET"
build_rust "$SIM_ARM_TARGET"
build_rust "$SIM_X86_TARGET"

echo "==> Compiling shared C bootstrap for Apple slices"
build_bridge_object \
    iphoneos \
    "arm64-apple-ios${IOS_MIN_VERSION}" \
    "$OUT/device/nulang_mobile_host.o"
build_bridge_object \
    iphonesimulator \
    "arm64-apple-ios${IOS_MIN_VERSION}-simulator" \
    "$OUT/simulator-arm64/nulang_mobile_host.o"
build_bridge_object \
    iphonesimulator \
    "x86_64-apple-ios${IOS_MIN_VERSION}-simulator" \
    "$OUT/simulator-x86_64/nulang_mobile_host.o"

echo "==> Combining Rust runtime and C bootstrap"
combine_static_library \
    "$CARGO_TARGET_DIR/$DEVICE_TARGET/release/libnulang.a" \
    "$OUT/device/nulang_mobile_host.o" \
    "$OUT/device/libnulang_mobile.a"
combine_static_library \
    "$CARGO_TARGET_DIR/$SIM_ARM_TARGET/release/libnulang.a" \
    "$OUT/simulator-arm64/nulang_mobile_host.o" \
    "$OUT/simulator-arm64/libnulang_mobile.a"
combine_static_library \
    "$CARGO_TARGET_DIR/$SIM_X86_TARGET/release/libnulang.a" \
    "$OUT/simulator-x86_64/nulang_mobile_host.o" \
    "$OUT/simulator-x86_64/libnulang_mobile.a"

echo "==> Creating universal simulator archive"
lipo -create \
    "$OUT/simulator-arm64/libnulang_mobile.a" \
    "$OUT/simulator-x86_64/libnulang_mobile.a" \
    -output "$OUT/simulator/libnulang_mobile.a"

cp "$ROOT/include/nulang_embed.h" "$OUT/headers/"
cp "$ROOT/include/nulang_mobile_actions.h" "$OUT/headers/"
cp "$ROOT/include/nulang_mobile_host.h" "$OUT/headers/"
cp "$ROOT/platforms/ios/NulangMobileRuntime/CNulangMobile.h" "$OUT/headers/"
cp "$ROOT/platforms/ios/NulangMobileRuntime/module.modulemap" "$OUT/headers/"

echo "==> Creating NulangMobile.xcframework"
xcodebuild -create-xcframework \
    -library "$OUT/device/libnulang_mobile.a" \
    -headers "$OUT/headers" \
    -library "$OUT/simulator/libnulang_mobile.a" \
    -headers "$OUT/headers" \
    -output "$OUT/NulangMobile.xcframework"

echo "==> Verifying produced slices"
lipo -info "$OUT/device/libnulang_mobile.a"
lipo -info "$OUT/simulator/libnulang_mobile.a"

if ! find "$OUT/NulangMobile.xcframework" -name module.modulemap -print -quit | grep -q .; then
    echo "XCFramework is missing module.modulemap" >&2
    exit 1
fi

echo "==> Packaging XCFramework proof artifact"
(
    cd "$OUT"
    ditto -c -k --sequesterRsrc --keepParent \
        NulangMobile.xcframework \
        NulangMobile.xcframework.zip
    shasum -a 256 NulangMobile.xcframework.zip
)

echo "==> Creating local Swift runtime package"
swift_package="$OUT/NulangMobileRuntimePackage"
mkdir -p "$swift_package/Sources/NulangMobileRuntime"
cp \
    "$ROOT/platforms/ios/NulangMobileRuntime/Package.swift.template" \
    "$swift_package/Package.swift"
cp \
    "$ROOT/platforms/ios/NulangMobileRuntime/Sources/NulangMobileRuntime/NulangMobileRuntime.swift" \
    "$swift_package/Sources/NulangMobileRuntime/"
ditto \
    "$OUT/NulangMobile.xcframework" \
    "$swift_package/NulangMobile.xcframework"

echo "==> Compiling generated Swift runtime package for iOS Simulator"
(
    cd "$swift_package"
    xcodebuild \
        -scheme NulangMobileRuntime \
        -destination 'generic/platform=iOS Simulator' \
        -derivedDataPath "$OUT/swift-derived-data" \
        CODE_SIGNING_ALLOWED=NO \
        build
)

echo "==> Packaging Swift runtime proof artifact"
(
    cd "$OUT"
    ditto -c -k --sequesterRsrc --keepParent \
        NulangMobileRuntimePackage \
        NulangMobileRuntimePackage.zip
    shasum -a 256 NulangMobileRuntimePackage.zip
)

echo
echo "Apple mobile runtime: $OUT/NulangMobile.xcframework"
echo "Swift runtime package: $OUT/NulangMobileRuntimePackage.zip"
