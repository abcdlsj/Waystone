# Waystone 开发进度

> 面向接手者的现状说明。设计口径以 [`design.md`](design.md) 为准，本文件只讲**已经落地的代码、验证方式、已知问题、下一步**。
>
> 最后更新：见 git log `HEAD`。

## 1. 一句话状态

端到端骨架已跑通到「设备配对 → 信令 → WebRTC 协商 → 媒体包双向传输」，**唯一没闭环的是 iOS 侧上屏**（收到首帧后画面仍黑，原因已定位，见第 5 节）。Windows 被控端尚未开始。

## 2. 仓库结构

```
crates/waystone-proto    协议层：设备身份、输入帧、配对 URI、信令消息（无重依赖，iOS 也编）
crates/waystone-rtc      WebRTC 引擎：str0m 驱动 + 自写 STUN + sans-io TURN + C ABI
crates/waystoned         自部署服务端：SQLite + HTTPS 面板 + WSS 信令 + 内嵌 TURN
host/macos/waystone-host macOS 被控端：ScreenCaptureKit + VideoToolbox + CoreGraphics 输入注入
client-ios               iOS 控制端：SwiftUI + Metal + VideoToolbox 硬解 + Opus 播放
docs/design.md           原始设计文档（中文，产品口径）
```

技术栈偏离设计文档的唯一一处：**iOS 用原生 Swift 而非 Flutter**（已与需求方确认）。原因：音视频本来就必须绕开 Flutter 走原生，且 Flutter 会额外引入 1.2GB SDK + Pod 链路。

## 3. 已完成的模块

### 3.1 `waystone-proto`（14 个单测）

- `device.rs`：`DeviceId` 由 Ed25519 公钥派生（`sha256(pubkey)[..16]`），`Role`/`Platform`/`Viewport`
- `identity.rs`：挑战签名载荷格式 `waystone-auth-v1|<server_id>|<nonce>`，与 iOS `CryptoKit` 逐字节一致
- `input.rs`：手写二进制输入帧（鼠标移动/按键/滚轮/键盘/文本/释放全部/虚拟手柄），`input-fast`（不可靠无序）与 `input-reliable`（可靠有序）分流
- `pairing.rs`：`waystone://pair?ep=…&turn=…&fp=…&tk=…` 解析与渲染，IPv6 端点支持
- `signal.rs`：JSON 信令（`type` 为 tag），配对/认证/会话/ICE/退出/控制全量

### 3.2 `waystone-rtc`（17 单测 + 2 端到端 + 3 FFI）

- `engine.rs`：**单所有者驱动**。str0m 要求「每次 mutation 后必须 poll 到 Timeout」，所以 `Rtc` 只被一个 task 持有，外部只发命令、只收回调，不可能破坏该不变量
  - 候选收集：host（全部物理网卡，跳过隧道）/ srflx（自写 STUN）/ relay（TURN）
  - `net.rs`：从 `recvmsg` 辅助数据读目的地址——通配 socket 必须回填目的地址，否则 ICE 会把所有连通性检查当作非法丢弃（这是之前黑屏的主因之一）
  - 中继回退时自动把视频码率钳到 `relay_bitrate_ceiling`
  - `Event::FrameDropped`：会话未就绪时丢帧会通知宿主重试关键帧
- `stun.rs`：RFC 5389 编解码（XOR 地址、长期凭据、`MESSAGE-INTEGRITY`、`FINGERPRINT`），类型编码对齐 RFC 表格
- `turn.rs`：sans-io TURN 分配状态机（401 挑战 → Allocate → ChannelBind → Refresh），不持有 socket，因此能与 ICE 共用同一个 UDP 端口
- `ffi.rs`：C ABI（控制事件走 JSON，媒体事件走 16 字节头 + 裸载荷），iOS 与 macOS 共用同一份 ICE/DTLS/RTP 实现

### 3.3 `waystoned`（16 单测 + 3 信令集成 + 1 全链路端到端）

单二进制，两个端口：

- `TCP 8443`：HTTPS 面板（配对二维码、设备列表）+ `/ws` 信令
- `UDP 3478`：内嵌 STUN/TURN

| 文件 | 职责 |
|---|---|
| `instance.rs` | 首次启动生成自签证书 + 稳定 `server_id`，私钥 0600；指纹供客户端固定 |
| `store.rs` | SQLite（WAL、busy_timeout）：实例、设备池、一次性配对令牌。令牌与角色绑定，手机无法把自己注册成 host |
| `challenge.rs` | 每连接 nonce（60s TTL、用后即焚）+ Ed25519 验签 |
| `hub.rs` | 设备注册表、会话路由、双向 teardown 通知；同一设备重连踢掉旧连接 |
| `turnserver.rs` | 内嵌 TURN，coturn `use-auth-secret` 兼容凭据（服务端不存每客户端密码） |
| `http.rs` | 面板、`/api/{status,pair,devices,claim}`、WSS 升级（Origin 门禁） |

`tests/end_to_end.rs` 是本仓库最有价值的测试：真实 HTTPS/WSS 服务端 + 真实两端引擎，跑完 配对→认证→SessionStart→SDP/ICE 交换→HEVC 帧→输入帧→挂断 全链路。

### 3.4 `host/macos/waystone-host`（12 单测）

- `swift/Capture.swift`（build.rs 调 swiftc 编成静态库）：
  - ScreenCaptureKit 采集（NV12，IO 表面直通）
  - VideoToolbox HEVC/H.264：`RealTime`、禁 B 帧、`MaxKeyFrameInterval=0`、无 look-ahead
  - AVCC → Annex-B 转换，每个关键帧前置 VPS/SPS/PPS
  - AudioToolbox Opus（48kHz 立体声，20ms 帧长）；设备自带的 Opus 编码器，无外部依赖
  - **缓存最后一帧并在需要时重编码**：ScreenCaptureKit 只在画面变化时出帧，静态桌面下新接入的观众会永远等不到画面，这是必须的补偿
  - CoreGraphics 输入注入，含 `ReleaseAll` 兜底（断连不会留下卡住的 Ctrl）
  - 电源断言：显示器休眠时 SCK 报告 0 个显示器，会话直接失败
- `src/signal.rs`：指纹固定（不走 CA 链）的 WSS 客户端
- `src/config.rs`：设备密钥、配置持久化（0600）
- CLI：`pair <uri>` / `run` / `status` / `capture-test`（`capture-test` 用于在配对前验证录屏权限）

### 3.5 `client-ios`（9 个单测）

- 首页（设备池 + 扫码 + 粘贴兜底）、串流页（Metal 层 + 手势 + 悬浮球 + 三套胶囊 + 状态胶囊）
- `GestureEngine`：设计文档里「点按 / 滑移悬停 / 停顿拖拽 / 双指右键」状态机，9 个单测覆盖每个分支（尤其「悬停后抬手绝不点击」这条防误出牌的规则）
- `VideoDecoder`：Annex-B → AVCC + `CMFormatDescription`，解码直出 Metal
- `AudioPlayer`：`AVAudioEngine` + 自建 20ms 环形缓冲
- `SignalingClient`：`URLSessionWebSocketTask` + 证书指纹固定
- codec 自适应：`VTIsHardwareDecodeSupported(HEVC)` 决定协商 HEVC 还是 H.264（模拟器无 HEVC 解码器）

## 4. 验证方式（接手者请照做）

```bash
# 全量测试（约 10 秒）
cargo test --workspace

# 静态检查（当前 0 warning）
cargo clippy --workspace --all-targets

# 启动服务端
cargo run -p waystoned -- --data-dir /tmp/wsdata --listen 0.0.0.0:8443 \
  --endpoint <本机IP>:8443 --no-turn

# 生成配对二维码 / 文本
curl -sk -X POST "https://127.0.0.1:8443/api/pair?role=host"
curl -sk -X POST "https://127.0.0.1:8443/api/pair?role=controller"

# 被控端配对并运行
cargo run -p waystone-host -- --state-dir /tmp/wsstate pair "<waystone://pair?...>"
cargo run -p waystone-host -- --state-dir /tmp/wsstate run

# 先在无会话状态下验证录屏权限（会输出采集到的帧数）
cargo run -p waystone-host -- --state-dir /tmp/wsstate capture-test --seconds 3

# iOS（xcodegen + 预先构建 Rust 静态库）
cd client-ios && ./scripts/build-rust.sh release
xcodegen generate
xcodebuild -project Waystone.xcodeproj -scheme Waystone \
  -sdk iphonesimulator -destination 'platform=iOS Simulator,name=<设备名>' build
# 模拟器无法扫码，用环境变量注入配对 URI（仅 DEBUG 生效）
SIMCTL_CHILD_WAYSTONE_PAIR_URI="waystone://pair?..." \
SIMCTL_CHILD_WAYSTONE_AUTO_CONNECT=1 xcrun simctl launch <设备> dev.waystone.app
```

## 5. 已知问题（按优先级）

### P0 — iOS 收到首帧后画面仍黑

**现象**：主机 `media ready` → 请求关键帧 → 帧已发出；iOS `media negotiated` → `transport connected` → 收到音频包 / 首帧 → 之后无后续帧 → 50s 后 DTLS 超时。

**已定位的两个独立原因**：

1. **主机端 PT 匹配时机**：`engine.rs:876 payload_type()` 走 `writer.payload_params()`，host 侧刚 `accept_offer` 后 remote PT 列表可能尚未就绪，导致 `H264 is not negotiated for Mid(...)` 而丢帧。修法：把 PT 在 `MediaReady` 时缓存，或改用 `rtc.media(mid).remote_pts()` 直查。
2. **候选集收敛**：主机同时通告了 `10.23.x`、`192.168.x`、`127.0.0.1` 等多个 host 候选，模拟器与宿主共享网络命名空间，ICE 有时选中 `127.0.0.1` 之外的地址后单向可达。修法：给 `EngineConfig` 加候选白名单 / 优先级，或让主机按 `--endpoint` 的网段只通告相关候选。

> 这两条都不需要改架构，属于局部修补。建议改完后用 `WAYSTONE_TRACE_NET=1` 复跑观察 `rx/tx`。

### P1 — 未实现（设计文档要求，MVP 范围内）

| 项 | 说明 |
|---|---|
| Windows 被控端 | 完全未开始。含 Session 0 服务 + 用户态采集进程、DXGI Desktop Duplication、NVENC/AMF/oneVPL、WASAPI loopback、UAC/锁屏桌面切换、IddCx 虚拟屏、断连锁屏还原 |
| IddCx 虚拟显示器 | macOS 端未做（用真实屏幕尺寸），Windows 端未做 |
| 中继限速实测 | 代码已实现（`relay_bitrate_ceiling`），但未在真实 TURN 路径上验证 |
| 静音接管 | 设计文档要求会话时把被控端物理声卡静音；未实现 |
| 自动锁屏 | 会话结束时锁屏；macOS 未实现 |
| 音频静音联动 | iOS 侧静音开关已做，被控端静音未做 |
| 敏锐度校准 | 手势阈值（160ms / 8pt）未做真机调参，模拟器无法验证手感 |

### P2 — 工程化缺口

- `waystoned` 无 Docker 镜像 / systemd 单元
- macOS 被控端只有 CLI，无 `.app` 打包、无 LaunchAgent
- 无 CI（当前靠本地 `cargo test` / `xcodebuild`）
- 无日志落盘与轮转（只有 stderr）
- 设计文档里的 `/data` 目录约定未落实到默认路径

## 6. 代码约定

- **代码 / 注释 / 文档 / commit message 一律英文**；只有 `docs/*.md` 可用中文
- 每个模块落地即提交，commit 信息说明「为什么」而非「改了什么」
- Rust 侧 `cargo fmt` + `cargo clippy --workspace --all-targets` 必须干净
- 测试优先写在被测模块内部（`#[cfg(test)] mod tests`），跨模块才放 `tests/`

## 7. 接手建议

1. 先跑 `cargo test --workspace` 和 `crates/waystoned/tests/end_to_end.rs`，理解全链路
2. 修 P0 两条，用模拟器验证到「画面出现」
3. Windows 被控端按 `host/macos` 的分层复制：`signal`（信令）/ `capture`（媒体）/ `input`（注入）三层，协议层完全复用 `waystone-proto` 与 `waystone-rtc`
