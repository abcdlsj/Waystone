#!/usr/bin/env bash
# Builds `waystone-rtc` for Android and links the JNI wrapper into
# `client-android/app/src/main/jniLibs/<abi>/libwaystone_rtc.so`.
#
# Usage: scripts/build-rust.sh [release|debug] [arm64-v8a|x86_64|all]
#
# Prerequisites:
#   - Android NDK (`ANDROID_NDK_HOME` or `$ANDROID_HOME/ndk/<latest>`)
#   - Rust target(s): `rustup target add aarch64-linux-android` (and optionally `x86_64-linux-android`)

set -euo pipefail

PROFILE="${1:-release}"
ABI_FILTER="${2:-arm64-v8a}"
API_LEVEL=29

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
NATIVE_DIR="$ROOT/client-android/native"
OUT_BASE="$ROOT/client-android/app/src/main/jniLibs"

find_ndk() {
  if [ -n "${ANDROID_NDK_HOME:-}" ] && [ -d "$ANDROID_NDK_HOME" ]; then
    echo "$ANDROID_NDK_HOME"
    return
  fi
  local sdk="${ANDROID_HOME:-${ANDROID_SDK_ROOT:-$HOME/Library/Android/sdk}}"
  if [ -d "$sdk/ndk" ]; then
    local latest
    latest="$(ls -1 "$sdk/ndk" | sort -V | tail -n 1)"
    if [ -n "$latest" ]; then
      echo "$sdk/ndk/$latest"
      return
    fi
  fi
  echo "error: Android NDK not found; set ANDROID_NDK_HOME or install NDK via Android Studio" >&2
  exit 1
}

NDK="$(find_ndk)"
HOST_OS="$(uname -s | tr '[:upper:]' '[:lower:]')"
HOST_ARCH="$(uname -m)"
if [ "$HOST_OS" = "darwin" ]; then
  HOST_TAG="darwin-x86_64"
  if [ -d "$NDK/toolchains/llvm/prebuilt/darwin-arm64" ]; then
    HOST_TAG="darwin-arm64"
  fi
elif [ "$HOST_OS" = "linux" ]; then
  HOST_TAG="linux-$HOST_ARCH"
else
  echo "error: unsupported host OS: $HOST_OS" >&2
  exit 1
fi

TOOLCHAIN="$NDK/toolchains/llvm/prebuilt/$HOST_TAG/bin"
if [ ! -d "$TOOLCHAIN" ]; then
  echo "error: NDK LLVM toolchain not found at $TOOLCHAIN" >&2
  exit 1
fi

if [ "$PROFILE" = "release" ]; then
  TARGET_DIR="release"
  CARGO_FLAGS=(--release)
  OPT_FLAG="-O2"
else
  TARGET_DIR="debug"
  CARGO_FLAGS=()
  OPT_FLAG="-O0 -g"
fi

build_abi() {
  local abi="$1"
  local rust_target clang_prefix cargo_env
  case "$abi" in
    arm64-v8a)
      rust_target="aarch64-linux-android"
      clang_prefix="aarch64-linux-android${API_LEVEL}"
      cargo_env="AARCH64_LINUX_ANDROID"
      ;;
    x86_64)
      rust_target="x86_64-linux-android"
      clang_prefix="x86_64-linux-android${API_LEVEL}"
      cargo_env="X86_64_LINUX_ANDROID"
      ;;
    *)
      echo "error: unsupported ABI: $abi" >&2
      exit 1
      ;;
  esac

  local cc="$TOOLCHAIN/${clang_prefix}-clang"
  local ar="$TOOLCHAIN/llvm-ar"
  local strip_bin="$TOOLCHAIN/llvm-strip"

  echo "==> [$abi] building waystone-rtc ($rust_target, $PROFILE)"
  (
    cd "$ROOT"
    export "CC_${rust_target//-/_}=$cc"
    export "AR_${rust_target//-/_}=$ar"
    export "CARGO_TARGET_${cargo_env}_LINKER=$cc"
    cargo build "${CARGO_FLAGS[@]}" -p waystone-rtc --target "$rust_target"
  )

  local static_lib="$ROOT/target/$rust_target/$TARGET_DIR/libwaystone_rtc.a"
  local out_dir="$OUT_BASE/$abi"
  local out_so="$out_dir/libwaystone_rtc.so"
  mkdir -p "$out_dir"

  echo "==> [$abi] linking JNI bridge -> $out_so"
  "$cc" $OPT_FLAG -shared -fPIC \
    -I"$NATIVE_DIR" \
    "$NATIVE_DIR/waystone_rtc_jni.c" \
    "$static_lib" \
    -llog -lm -ldl \
    -Wl,--gc-sections \
    -o "$out_so"

  if [ "$PROFILE" = "release" ] && [ -x "$strip_bin" ]; then
    "$strip_bin" --strip-unneeded "$out_so"
  fi
}

case "$ABI_FILTER" in
  all)
    build_abi arm64-v8a
    build_abi x86_64
    ;;
  arm64-v8a|x86_64)
    build_abi "$ABI_FILTER"
    ;;
  *)
    echo "error: ABI must be arm64-v8a, x86_64, or all" >&2
    exit 1
    ;;
esac

echo "==> done: $OUT_BASE"
