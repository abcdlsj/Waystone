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

FLAGS=()
if [ "$PROFILE" = "release" ]; then
  FLAGS+=(--release)
  TARGET_DIR="release"
else
  TARGET_DIR="debug"
fi

echo "==> device slice"
cargo build -p waystone-rtc --target aarch64-apple-ios "${FLAGS[@]}"

echo "==> simulator slice"
cargo build -p waystone-rtc --target aarch64-apple-ios-sim "${FLAGS[@]}"

rm -rf "$OUT/WaystoneRTC.xcframework"
mkdir -p "$OUT/include"
cp "$ROOT/client-ios/Waystone/Transport/waystone_rtc.h" "$OUT/include/waystone_rtc.h"

echo "==> xcframework"
xcodebuild -create-xcframework \
  -library "$ROOT/target/aarch64-apple-ios/$TARGET_DIR/libwaystone_rtc.a" \
  -headers "$OUT/include" \
  -library "$ROOT/target/aarch64-apple-ios-sim/$TARGET_DIR/libwaystone_rtc.a" \
  -headers "$OUT/include" \
  -output "$OUT/WaystoneRTC.xcframework" >/dev/null

echo "==> done: $OUT/WaystoneRTC.xcframework"
