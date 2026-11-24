# Waystone 开发进度

> 面向接手者的现状说明。产品口径以 [`design.md`](design.md) 为准，最近一次验收见 [`ACCEPTANCE.md`](ACCEPTANCE.md)。本文件只讲已经落地的代码、怎么验证、还缺什么。

## 1. 状态

macOS 被控端 + iOS 控制端 + 自部署服务端的 MVP 闭环已完成：

- 配对、信令、WebRTC（直连优先，TURN 兜底）、音视频、双轨输入都已实现。
- 设计文档 5.2–5.4 的手势、热键、悬浮球和弱网处理也已实现。
- 在模拟器上有一条端到端 UI 验收用例。

Windows 被控端未开始。

## 2. 仓库结构

```
crates/waystone-proto     协议：设备身份、配对链接、JSON 信令、二进制输入帧（HID 键码）
crates/waystone-rtc       传输：str0m 单所有者引擎 + sans-io STUN/TURN + C ABI（iOS 与 macOS 共用）
crates/waystoned          服务端：HTTPS 面板 + WSS 信令 + 内嵌 TURN + SQLite
host/macos/waystone-host  macOS 被控端：Rust 主体 + Swift 静态库（采集/编码/注入/系统）
client-ios                iOS 控制端：SwiftUI + VideoToolbox + Metal + AVAudioEngine
docs/                     设计文档、本文件、验收报告与截图
```

偏离设计文档的地方：iOS 用原生 Swift，不用 Flutter（已与需求方确认）。音视频本来就必须走原生，Flutter 只会多一层桥接。

## 3. 模块

### 3.1 `waystone-proto`

| 文件 | 职责 |
|---|---|
| `device.rs` | `DeviceId`（Ed25519 公钥派生）、`Role`、`Platform`、`Viewport`、`VideoCodec` |
| `identity.rs` | 挑战签名载荷 `waystone-auth-v1\|<server_id>\|<nonce>`，与 iOS CryptoKit 逐字节一致 |
| `pairing.rs` | `waystone://pair?ep=…&turn=…&fp=…&tk=…` 解析与生成 |
| `signal.rs` | JSON 信令；`busy`、`relay_bitrate_ceiling`、`ByeReason` 等 |
| `input.rs` | 输入帧编解码；`keys` 模块是 HID usage 常量；鼠标移动走不可靠通道，其余走可靠通道 |

### 3.2 `waystone-rtc`

- `engine.rs`：`Rtc` 只由一个任务持有，外部只发命令、只收事件。str0m 要求每次修改状态后都要 poll 到超时，这样安排就不可能违反这条规则。负责收集候选（host / srflx / relay），并有仅中继模式。走中继时把码率钳到上限。
- `net.rs`：从 `recvmsg` 辅助数据读出目的地址。绑定 `0.0.0.0` 的 socket 不这样做，ICE 会把连通性检查当成非法包丢掉。
- `stun.rs`、`turn.rs`：RFC 5389 编解码；TURN 分配状态机不持有 socket，所以能和 ICE 共用一个 UDP 端口。
- `ffi.rs`：C ABI。控制事件用 JSON，媒体事件用 16 字节头加原始载荷；`ws_engine_stop` 返回后不再有回调。

### 3.3 `waystoned`

| 文件 | 职责 |
|---|---|
| `instance.rs` / `tls.rs` | 首次启动生成自签证书和 `server_id`，私钥 0600；客户端固定证书指纹 |
| `store.rs` | SQLite（WAL）：实例、设备、一次性配对令牌（单条 `UPDATE … RETURNING` 消费） |
| `auth.rs` | 管理员口令一次性认领，Argon2id，`HttpOnly; Secure; SameSite=Strict` cookie |
| `http.rs` | 面板、`/api/*`、同源校验、WSS（认证超时、ping/RTT、64 KiB 上限、慢客户端断开） |
| `hub.rs` | 设备注册、会话路由、占用检查、参与者校验、吊销踢线 |
| `turnserver.rs` | 内嵌 TURN，coturn `use-auth-secret` 兼容凭据，检查过期，拒绝中继到回环地址 |
| `lan.rs` | 未指定 `--endpoint` 时自动选择主 LAN IPv4 |
| `pairing.rs` | 一次性令牌、配对链接与二维码 |
| `main.rs` | `waystoned pair --role host\|controller`：直接从数据目录生成配对链接 |

部署见 `crates/waystoned/README.md` 和 `Dockerfile`。

### 3.4 macOS 被控端

| 文件 | 职责 |
|---|---|
| `main.rs` | CLI：`pair <uri> [--name]`、`run [--no-lock] [--no-mute]`、`status`、`capture-test` |
| `agent.rs` | 信令连接、退避重连、一次只服务一个会话 |
| `session.rs` | 会话生命周期：`start` / `on_signal` / `on_control` / `finish`；10 秒重连窗口；关键帧请求间隔 250 ms，晚到的推迟执行，不丢弃 |
| `capture.rs` | 屏幕采集句柄，Drop 时停止 |
| `plan.rs` | 编码尺寸与码率：保持桌面宽高比，不超过面板像素，长边最大 3840，码率按像素数 × 帧率 |
| `input.rs` / `keymap.rs` | 注入线程：最短按住 15 ms、连击计数、修饰键、全部释放；HID 到 macOS 虚拟键码 |
| `system.rs` | 显示器、锁屏、扬声器静音守卫、辅助功能权限检查 |
| `config.rs` | 设备密钥与配置（0600 原子写入）；`locks_after` 决定哪些断开原因要锁屏 |
| `swift/` | ScreenCaptureKit 采集（静止 500 ms 后以 5 fps 保活）、VideoToolbox H.264/HEVC、AudioToolbox Opus、CGEvent 注入、系统调用 |

断开时的收尾顺序固定为：释放输入 → 停采集 → 关引擎 → 恢复扬声器 → 按原因锁屏。用户主动挂断和主机出错不锁屏，网络超时和对端离线按配置 `lock_on_disconnect` 决定，「断开并锁屏」一定锁屏。

### 3.5 iOS 控制端

分层与各层职责见 [`ACCEPTANCE.md`](ACCEPTANCE.md) 2.2 节。扩展点：

- **新热键预设**：在 `Input/HotkeyPreset.swift` 加一个静态常量并放进 `all`，不用改视图代码。
- **新触控方式**：实现 `TouchEngine` 协议，在 `InputController.makeEngine` 里加一个分支。
- **新信令消息**：`Protocol/Wire.swift` 加一个 case；会话相关的消息由 `PoolConnection` 按会话 id 转给 `SessionMessageHandler`。

codec 协商：控制端用 `VTIsHardwareDecodeSupported` 判断能否硬解 HEVC（模拟器不能），然后在 `session_start.video_codec` 里告诉被控端。被控端据此同时决定 SDP 和编码器。

## 4. 验证

```bash
cargo test --workspace                       # 123 个，约 10 秒
cargo clippy --workspace --all-targets       # 0 警告
cargo fmt --all --check

cd client-ios
scripts/build-rust.sh release                # 生成 Vendor/WaystoneRTC.xcframework
xcodegen generate
xcodebuild -scheme Waystone -destination 'platform=iOS Simulator,name=<模拟器>' test   # 32 个单测
```

端到端验收（需要先启动服务端和被控端）见 [`ACCEPTANCE.md`](ACCEPTANCE.md) 3.1 节。

模拟器不能扫码，DEBUG 构建支持两种注入配对链接的方式：

- 环境变量 `WAYSTONE_PAIR_URI`（加 `WAYSTONE_AUTO_CONNECT=1` 会自动连第一台主机）。
- 文件 `Application Support/Waystone/pair.txt`。

排障：

- 换了服务端数据目录，证书指纹就会变化。这时要在 App 首页用「移除此服务器」删掉旧服务器，或卸载 App。
- 被控端加 `RUST_LOG=waystone_host=debug` 会逐条打印收到的输入事件。
- `WAYSTONE_DUMP_FRAMES=<dir>` 会把编码后的前 40 帧写到磁盘。
- iOS 的 DEBUG 构建每 2 秒打印一次收包数、解码数和跳帧数（`dev.waystone.app` / `session` 分类）。

## 5. 还缺什么

### MVP 范围内

| 项 | 说明 |
|---|---|
| Windows 被控端 | 未开始：Session 0 服务 + 用户态采集进程、DXGI、NVENC/AMF/oneVPL、WASAPI loopback、UAC / 锁屏桌面切换、IddCx。协议和传输层直接复用，分层照搬 `host/macos` |
| 虚拟显示器 | macOS 用真实屏幕比例，画面加黑边 |
| 真机调参 | 手势阈值（160 ms / 8 pt / 双指 200 ms）、触控板加速曲线只在模拟器上跑过 |

### 工程化

- macOS 被控端只有 CLI，没有 `.app` 打包和 LaunchAgent。
- 没有 CI。
- 日志只输出到 stderr，不落盘，也不轮转。
- 旧版本用 SHA-256 存的管理员口令无法登录，需要清空 `admin_token_hash` 后重新认领，没有迁移脚本。

### 之后的阶段（设计文档第 7 节）

手柄（ViGEm）、唤醒开机、游戏启动台、分享与桌面控制端，均未开始。

## 6. 代码约定

- 代码、注释、commit message 一律英文；只有 `docs/*.md` 可用中文。
- commit message 说明「为什么改」。
- Rust 侧 `cargo fmt` 和 `cargo clippy --workspace --all-targets` 必须干净。
- 测试优先写在被测模块内（`#[cfg(test)] mod tests`），跨模块的才放 `tests/`。
