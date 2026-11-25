# Waystone Android Controller (`client-android`)

Native Android controller built with Kotlin, Jetpack Compose, `MediaCodec`, `AudioTrack`, CameraX + ZXing, and the shared Rust WebRTC transport (`crates/waystone-rtc`).

The UI mirrors `client-ios` 1:1 with the same pure-black OLED monochrome console aesthetic, touch/trackpad engines, hotkey presets, floating ball, live path telemetry, and mid-stream resolution/FPS controls.

## Prerequisites

- Android Studio (Ladybug or newer) or Gradle 8.11+ with Android SDK 35 (`minSdk 29`)
- Android NDK installed via Android Studio SDK Manager (`ANDROID_NDK_HOME` or `$ANDROID_HOME/ndk/<version>`)
- Rust target for Android:
  ```sh
  rustup target add aarch64-linux-android
  # Optional, for x86_64 emulator:
  rustup target add x86_64-linux-android
  ```

## Build

1. Build the shared Rust WebRTC transport and JNI bridge (`libwaystone_rtc.so`):

   ```sh
   client-android/scripts/build-rust.sh release
   # Or build both arm64-v8a and x86_64:
   client-android/scripts/build-rust.sh release all
   ```

   This writes `client-android/app/src/main/jniLibs/<abi>/libwaystone_rtc.so`.

2. Open `client-android` in Android Studio and run, or build with Gradle:

   ```sh
   cd client-android
   gradle :app:assembleDebug
   gradle :app:testDebugUnitTest
   ```

## Architecture

- `native/waystone_rtc_jni.c` — Minimal C99 JNI shim around `crates/waystone-rtc/src/ffi.rs` (`waystone_rtc.h`), reusing the exact `CallbackGate` contract and zero-copy media header shared with iOS.
- `protocol/` — JSON signaling (`Wire`), `waystone://pair` URI parser (`PairingLink`), and binary HID input frames (`InputEvent`).
- `pool/` — Ed25519 device identity (BouncyCastle), JSON server store, OkHttp WSS signaling client with leaf-certificate SHA-256 pinning, and reconnecting `PoolConnection`.
- `media/` — Hardware `MediaCodec` H.265/H.264 decoder rendering directly to `SurfaceView` and low-latency `MediaCodec` Opus + `AudioTrack` player.
- `input/` — `PictureGeometry`, `DirectTouchEngine`, `TrackpadEngine`, `HotkeyPreset` (`卡牌` / `视觉小说` / `日常` / `无`), and `InputController`.
- `ui/` — Pure-black monochrome Compose screens (`HomeScreen`, `PairingSheet`, `StreamScreen`, `ControlPanel`, `FloatingBall`, `HotkeyLayer`, `SoftKeyboard`).
