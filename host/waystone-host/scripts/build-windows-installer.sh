#!/bin/bash
# Builds the self-contained Windows setup wizard (`WaystoneSetup.exe`) from
# macOS or Linux using `zig` + `cargo-zigbuild`.
#
# Output: target/x86_64-pc-windows-gnu/release/WaystoneSetup.exe

set -euo pipefail
cd "$(dirname "$0")/../../.."
TARGET=x86_64-pc-windows-gnu
RELEASE_DIR="target/$TARGET/release"

rustup target add "$TARGET" >/dev/null

echo "[1/2] Building waystone-host.exe..."
cargo zigbuild --release -p waystone-host --target "$TARGET"

HOST_EXE="$(pwd)/$RELEASE_DIR/waystone-host.exe"
echo "[2/2] Building WaystoneSetup.exe with embedded host payload..."
WAYSTONE_HOST_EXE="$HOST_EXE" cargo zigbuild --release -p waystone-setup --target "$TARGET"

SETUP_OUT="$RELEASE_DIR/WaystoneSetup.exe"
cp "$RELEASE_DIR/waystone-setup.exe" "$SETUP_OUT"
ls -lh "$SETUP_OUT"
shasum -a 256 "$SETUP_OUT"
