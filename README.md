# Waystone (界石)

Self-hosted, low latency game streaming for light games: card battlers, tactics,
visual novels and daily-routine grinding.

The single design document lives in [`docs/design.md`](docs/design.md) (Chinese,
kept as the product spec). Everything else in this repository — code, comments,
commit messages and developer documentation — is English.

## Repository layout

```
crates/waystone-proto   Wire protocol shared by every component
crates/waystone-rtc     str0m session, ICE and bitrate plumbing
crates/waystoned        All-in-one daemon: admin panel, WSS signaling, TURN
host/macos              macOS host agent (ScreenCaptureKit + VideoToolbox)
client-ios              iOS controller (SwiftUI + VideoToolbox + Metal)
```

## Status

Early MVP work. Modules are landed in reviewable slices, each with its own
commit, so a partially implemented feature never blocks the tree from building.

| Area | State |
| --- | --- |
| `waystone-proto` | Protocol types, input frames, pairing URI |
| `waystone-rtc` | In progress |
| `waystoned` | In progress |
| `host/macos` | In progress |
| `client-ios` | In progress |

## Development

```sh
cargo test --workspace
```

iOS and macOS targets are built from Xcode; see `client-ios/README.md`.
