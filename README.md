# Waystone (界石)

Self-hosted, low-latency remote game streaming for light PC games: card battlers,
tactics, visual novels, and daily-routine play from an iPhone or iPad.

The product specification lives in [`docs/design.md`](docs/design.md) (Chinese).
Everything else in this repository — code, comments, commit messages, and
developer documentation — is English.

## Highlights

- **End-to-end WebRTC (`waystone-rtc`)**: Single-owner `str0m` engine with
  dual-stack IPv4/IPv6 ICE, UPnP/NAT-PMP/PCP port mapping, embedded TURN relay
  fallback, and **NAT3↔NAT4 birthday-attack UDP hole punching** that seamlessly
  upgrades relayed sessions to direct P2P on symmetric corporate Wi-Fi and 5G.
- **Self-hosted all-in-one server (`waystoned`)**: Single binary serving the
  HTTPS admin panel, Ed25519-authenticated WSS signaling, one-shot QR pairing
  tokens, and an embedded TURN relay with configurable relay bitrate ceiling.
- **macOS & Windows hosts (`waystone-host`)**:
  - **macOS**: ScreenCaptureKit + VideoToolbox (HEVC/H.264) + AudioToolbox Opus
    + `CGEvent` injection.
  - **Windows**: DXGI Desktop Duplication + **GPU HDR→SDR scRGB tone mapping**
    (reads Windows SDR white level) + 1 ms capture pacing + Media Foundation
    hardware encoding (NVENC/AMF/QSV) + WASAPI loopback Opus + `SendInput`
    injection + tray UI + Session 0 service & console worker for UAC/lock-screen
    and elevated windows.
- **Native iOS controller (`client-ios`)**: Pure-black monochrome console UI in
  SwiftUI, hardware VideoToolbox + Metal rendering, direct-touch & trackpad
  modes, game hotkey presets & virtual stick, live `P2P 直连` / `TURN 中继` +
  RTT/FPS telemetry, and live mid-stream resolution (`原生` / `720p` / `1080p` /
  `2K`) and frame-rate (`30` / `45` / `60 Hz`) switching.

## Repository layout

```
crates/waystone-proto   Wire protocol shared by every component
crates/waystone-rtc     str0m engine, STUN/TURN, port mapping, NAT3-NAT4 punch, C ABI
crates/waystoned        All-in-one daemon: admin panel, WSS signaling, embedded TURN
host/waystone-host      Host agent for macOS and Windows (tray + Session 0 service)
client-ios              Native iOS controller (SwiftUI + VideoToolbox + Metal)
```

## Status

| Area | State |
| --- | --- |
| `waystone-proto` | Done — device identity, pairing URI, JSON signaling, binary input frames, live resolution/fps controls |
| `waystone-rtc` | Done — `str0m` session, IPv4/IPv6 ICE, UPnP/PCP/NAT-PMP, embedded TURN client, NAT3↔NAT4 birthday punch |
| `waystoned` | Done — HTTPS admin panel, WSS signaling, SQLite store, embedded TURN server |
| `host/waystone-host` | Done — macOS & Windows (DXGI, GPU HDR→SDR, Media Foundation HEVC/H.264, tray, Session 0 service) |
| `client-ios` | Done — SwiftUI console UI, VideoToolbox/Metal, touch/trackpad/hotkeys, live path/resolution/fps controls |

See [`docs/STATUS.md`](docs/STATUS.md) for architecture details and remaining
roadmap items.

## Development

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
```

Cross-building targets (requires `cargo-zigbuild` for Linux/Windows):

```sh
# Linux server (static musl)
cargo zigbuild --release -p waystoned --target x86_64-unknown-linux-musl

# Windows host
cargo zigbuild --release -p waystone-host --target x86_64-pc-windows-gnu

# iOS WaystoneRTC.xcframework
client-ios/scripts/build-rust.sh release
```

For iOS app building and deployment, see [`client-ios/README.md`](client-ios/README.md).
For server deployment, see [`crates/waystoned/README.md`](crates/waystoned/README.md).
