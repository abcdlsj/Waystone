#!/bin/bash
# Cross-compiles the Windows host from macOS or Linux.
#
# Needs `zig` and `cargo-zigbuild` (`cargo install cargo-zigbuild`); zig is
# the C compiler and linker for the few C dependencies (aws-lc, ring).
# Output: target/x86_64-pc-windows-gnu/release/waystone-host.exe
#
# Usage: host/waystone-host/scripts/build-windows.sh [check]
#   check  runs clippy for the Windows target instead of building.

set -euo pipefail
cd "$(dirname "$0")/../../.."
TARGET=x86_64-pc-windows-gnu
rustup target add "$TARGET" >/dev/null

if [ "${1:-}" = "check" ]; then
  # clippy has no zig integration; point the C toolchain at zigbuild's
  # wrappers, which exist once zigbuild has run for this target.
  cargo zigbuild -p waystone-host --target "$TARGET" >/dev/null
  WRAPPERS=$(dirname "$(ls ~/Library/Caches/cargo-zigbuild/*/wrappers/*/zigcc-$TARGET-*.sh ~/.cache/cargo-zigbuild/*/wrappers/*/zigcc-$TARGET-*.sh 2>/dev/null | head -1)")
  export CC_x86_64_pc_windows_gnu=$(ls "$WRAPPERS"/zigcc-$TARGET-*.sh)
  export CXX_x86_64_pc_windows_gnu=$(ls "$WRAPPERS"/zigcxx-$TARGET-*.sh)
  export AR_x86_64_pc_windows_gnu="$WRAPPERS/ar"
  exec cargo clippy -p waystone-host --target "$TARGET" --all-targets -- -D warnings
fi

cargo zigbuild --release -p waystone-host --target "$TARGET"
ls -la "target/$TARGET/release/waystone-host.exe"
