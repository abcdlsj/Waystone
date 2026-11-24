#!/bin/bash
# Builds the shared WebRTC transport for iOS and packs it as an XCFramework.
#
# Usage: scripts/build-rust.sh [debug|release]
#
# The device and simulator slices are both arm64, so they cannot be merged with
# lipo; an XCFramework keeps them in separate platform slices and lets Xcode pick
# the right one.

set -euo pipefail

PROFILE="${1:-release}"
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
OUT="$ROOT/client-ios/Vendor"

cd "$ROOT"

# Must match deploymentTarget in project.yml. rustc and the cc crate (which
# builds the C parts of the crypto dependencies) both read it; without it they
# target the SDK version and the app link warns about every object file.
export IPHONEOS_DEPLOYMENT_TARGET=17.0

if [ "$PROFILE" = "release" ]; then
  TARGET_DIR="release"
  BUILD=(cargo build --release)
else
  TARGET_DIR="debug"
  BUILD=(cargo build)
fi

# macOS still ships bash 3.2, where `"${EMPTY[@]}"` is an unbound variable
# under `set -u`; the build flags are therefore expanded through this helper.
build_slice() {
  "${BUILD[@]}" -p waystone-rtc --target "$1"
}

echo "==> device slice"
build_slice aarch64-apple-ios

echo "==> simulator slice"
build_slice aarch64-apple-ios-sim

rm -rf "$OUT/WaystoneRTC.xcframework"
mkdir -p "$OUT/include"
cp "$ROOT/client-ios/Waystone/Stream/waystone_rtc.h" "$OUT/include/waystone_rtc.h"

echo "==> xcframework"
xcodebuild -create-xcframework \
  -library "$ROOT/target/aarch64-apple-ios/$TARGET_DIR/libwaystone_rtc.a" \
  -headers "$OUT/include" \
  -library "$ROOT/target/aarch64-apple-ios-sim/$TARGET_DIR/libwaystone_rtc.a" \
  -headers "$OUT/include" \
  -output "$OUT/WaystoneRTC.xcframework" >/dev/null

echo "==> done: $OUT/WaystoneRTC.xcframework"
