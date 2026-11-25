import Foundation
import UIKit

/// One stream from a host: negotiation, media, input, health and teardown.
///
/// The session is created when the user taps a host and is thrown away when it
/// ends; nothing outlives it, so a new session always starts clean.
///
/// Health follows design doc 5.4: a short stall keeps the last picture and
/// only turns the status dot yellow; after 3 s a non-blocking banner appears;
/// after 12 s the session ends with a cause the user can act on.
@MainActor
final class StreamSession: ObservableObject, Identifiable, SessionMessageHandler {
    enum Stage: Equatable {
        /// Waiting for the server, the host and the first picture.
        case connecting
        case live
        /// Media stopped; the last picture stays on screen.
        case stalled(since: Date)
        case ended(Ending)
    }

    /// Why a session ended, worded for the user. `nil` message means the user
    /// hung up and there is nothing to explain.
    struct Ending: Equatable {
        var title: String
        var message: String?

        static let hungUp = Ending(title: "已断开", message: nil)
    }

    struct Path: Equatable {
        var relayed = false
        var rttMs: UInt32?
    }

    let id = UUID()
    let host: Host
    let input: InputController

    @Published private(set) var stage: Stage = .connecting
    @Published private(set) var path = Path()
    @Published private(set) var progress = "正在连接…"
    @Published private(set) var liveFps = 0

    /// Seconds of silence before the recovery banner, and before giving up.
    static let bannerAfter: TimeInterval = 3
    static let giveUpAfter: TimeInterval = 12
    /// The first picture may take longer: capture and encoder start cold.
    static let firstPictureTimeout: TimeInterval = 20

    private let pool: PoolConnection
    private let preferences: StreamPreferences
    private let codec = Wire.VideoCodec.bestDecodable
    private let audio = AudioPlayer()
    private let pipeline: MediaPipeline
    private var transport: Transport?
    private var sessionId: String?
    private var outbox: [Wire.RtcSignal] = []
    private var startedAt = Date()
    private var healthTimer: Timer?
    private var lastKeyframeRequest = Date.distantPast
    private var lastFpsSample: (decoded: Int, at: Date) = (0, Date())
    #if DEBUG
    private var lastStats: (received: Int, decoded: Int, skipped: Int, failed: Int) = (0, 0, 0, 0)
    private var statsTicks = 0
    #endif

    init(host: Host, pool: PoolConnection, preferences: StreamPreferences) {
        self.host = host
        self.pool = pool
        self.preferences = preferences
        pipeline = MediaPipeline(codec: codec, audio: audio)
        input = InputController(mode: preferences.touchMode)
        input.sink = { [weak self] event in self?.transport?.send(event) }

        pipeline.onPictureSize = { [weak self] size in self?.input.videoSizeChanged(size) }
        pipeline.onKeyframeNeeded = { [weak self] in
            Task { @MainActor in self?.requestKeyframe() }
        }
    }

    var isEnded: Bool {
        if case .ended = stage { return true }
        return false
    }

    func attach(_ view: VideoView) {
        pipeline.attach(view)
    }

    // MARK: - Lifecycle

    func start() {
        startedAt = Date()
        lastFpsSample = (0, startedAt)
        let viewport = Self.viewport(resolution: preferences.resolution, fps: preferences.frameRate.rawValue)
        do {
            transport = try Transport(
                Transport.Configuration(
                    audio: preferences.sound != .off, codec: codec,
                    stun: pool.turn?.server ?? pool.instance.turn, turn: pool.turn),
                media: pipeline,
                onControl: { [weak self] event in
                    Task { @MainActor in self?.handle(event) }
                })
        } catch {
            end(Ending(title: "无法开始串流", message: error.localizedDescription))
            return
        }
        if preferences.sound != .off {
            audio.start(forceSound: preferences.sound == .on)
        }
        pool.requestSession(
            with: host, viewport: viewport, audio: preferences.sound != .off, codec: codec, handler: self)
        progress = "等待响应…"
        healthTimer = Timer.scheduledTimer(withTimeInterval: 0.5, repeats: true) { [weak self] _ in
            Task { @MainActor in self?.checkHealth() }
        }
    }

    /// Ends the session at the user's request.
    func hangUp(lock: Bool) {
        guard !isEnded else { return }
        input.releaseAll()
        if let sessionId {
            pool.send(.sessionBye(sessionId: sessionId, reason: lock ? .lockAndExit : .userHangup))
        }
        end(.hungUp)
    }

    // MARK: - Options

    func setFrameRate(_ rate: StreamPreferences.FrameRate) {
        preferences.frameRate = rate
        sendControl(.setFrameRate(fps: rate.rawValue))
    }

    func setResolution(_ resolution: StreamPreferences.Resolution) {
        preferences.resolution = resolution
        let target = Self.targetSize(for: resolution)
        sendControl(.setResolution(width: target.width, height: target.height))
    }

    func setSound(_ sound: StreamPreferences.Sound) {
        let wasOff = preferences.sound == .off
        preferences.sound = sound
        switch sound {
        case .off:
            audio.setMuted(true)
            sendControl(.setAudioEnabled(false))
        case .auto, .on:
            audio.start(forceSound: sound == .on)
            audio.setForceSound(sound == .on)
            audio.setMuted(false)
            if wasOff { sendControl(.setAudioEnabled(true)) }
        }
    }

    func setTouchMode(_ mode: InputController.Mode) {
        preferences.touchMode = mode
        input.setMode(mode)
    }

    func setPreset(_ preset: HotkeyPreset) {
        guard preset.id != preferences.presetId else { return }
        // Design doc 6.2: switching presets must not leave a key held.
        input.releaseAll()
        preferences.presetId = preset.id
    }

    func requestKeyframe() {
        guard Date().timeIntervalSince(lastKeyframeRequest) > 0.5 else { return }
        lastKeyframeRequest = Date()
        sendControl(.requestKeyframe)
    }

    // MARK: - SessionMessageHandler

    func sessionAccepted(id: String, codec: Wire.VideoCodec) {
        sessionId = id
        progress = "建立连接…"
        for signal in outbox { pool.send(.rtcSignal(sessionId: id, signal: signal)) }
        outbox.removeAll()
        // The stream starts with whatever the panel says now.
        if preferences.frameRate != .smooth { sendControl(.setFrameRate(fps: preferences.frameRate.rawValue)) }
    }

    func sessionRejected(reason: String) {
        let message = reason.contains("in use") || reason.contains("busy")
            ? "\(host.name) 正在被其他设备使用" : reason
        end(Ending(title: "无法连接", message: message))
    }

    func received(_ signal: Wire.RtcSignal) {
        switch signal {
        case .answer(let sdp): transport?.acceptAnswer(sdp)
        case .ice(let candidate): transport?.addCandidate(candidate)
        case .offer, .iceComplete: break
        }
    }

    func received(_ control: Wire.SessionControl) {}

    func sessionEnded(reason: Wire.ByeReason, detail: String?) {
        switch reason {
        case .userHangup, .lockAndExit:
            end(Ending(title: "会话已结束", message: "\(host.name) 结束了这次连接。"))
        case .peerOffline:
            end(Ending(title: "电脑已离线", message: "\(host.name) 当前已离线或休眠。"))
        case .networkTimeout:
            end(Ending(title: "网络中断", message: "与 \(host.name) 的连接超时。"))
        case .hostError, .unknown:
            end(Ending(title: "电脑端出错", message: detail ?? "\(host.name) 无法继续串流。"))
        }
    }

    func signalingLost() {
        // The media path is peer to peer and may well survive; health
        // monitoring decides, and attributes the cause if it does not.
        Log.session.info("signaling lost during session")
    }

    // MARK: - Transport

    private func handle(_ event: Transport.ControlEvent) {
        guard !isEnded else { return }
        switch event {
        case .localOffer(let sdp):
            relay(.offer(sdp: sdp))
        case .localCandidate(let candidate):
            relay(.ice(candidate: candidate))
        case .connected:
            progress = "等待画面…"
            requestKeyframe()
        case .disconnected:
            Log.session.info("ICE disconnected")
        case .path(let relayed, let rtt):
            path = Path(relayed: relayed, rttMs: rtt ?? path.rttMs)
        case .closed(let reason):
            end(Ending(title: "连接已关闭", message: reason))
        }
    }

    private func relay(_ signal: Wire.RtcSignal) {
        if let sessionId {
            pool.send(.rtcSignal(sessionId: sessionId, signal: signal))
        } else {
            // The engine offers before the server has assigned the session id.
            outbox.append(signal)
        }
    }

    private func sendControl(_ control: Wire.SessionControl) {
        guard let sessionId else { return }
        pool.send(.sessionControl(sessionId: sessionId, control: control))
    }

    // MARK: - Health

    private func checkHealth() {
        updateFps()
        logStats()
        let age = pipeline.pictureAge
        switch stage {
        case .connecting:
            if pipeline.hasPicture {
                stage = .live
            } else if Date().timeIntervalSince(startedAt) > Self.firstPictureTimeout {
                end(attributeFailure(firstPicture: true))
            }
        case .live:
            if age > Self.bannerAfter {
                stage = .stalled(since: Date().addingTimeInterval(-age))
                requestKeyframe()
            } else if age > 1.5 {
                // Host emits 5 fps keepalives even on a still desktop; >1.5 s
                // without a decoded picture means the decoder lost sync.
                requestKeyframe()
            }
        case .stalled:
            if age < 1 {
                stage = .live
            } else if age > Self.giveUpAfter {
                end(attributeFailure(firstPicture: false))
            } else {
                requestKeyframe()
            }
        case .ended:
            break
        }
    }

    /// Names the most likely cause, from what the signaling layer knows.
    private func attributeFailure(firstPicture: Bool) -> Ending {
        if !pool.isOnline {
            return Ending(title: "服务器连接超时", message: "无法连接服务器，请检查网络。")
        }
        if let current = pool.host(id: host.id), !current.online {
            return Ending(title: "主机已离线", message: "\(host.name) 已离线。")
        }
        if path.relayed {
            return Ending(title: "中继超时", message: "中继连接未恢复，请检查服务器网络。")
        }
        return firstPicture
            ? Ending(title: "连接超时", message: "未收到画面，可能被网络阻断。")
            : Ending(title: "网络中断", message: "画面中断超过 \(Int(Self.giveUpAfter)) 秒。")
    }

    private func end(_ ending: Ending) {
        guard !isEnded else { return }
        Log.session.info("session ended: \(ending.title, privacy: .public) \(ending.message ?? "", privacy: .public)")
        healthTimer?.invalidate()
        healthTimer = nil
        transport?.stop()
        transport = nil
        pipeline.shutdown()
        pool.releaseSession(self)
        stage = .ended(ending)
    }

    private func updateFps() {
        let now = Date()
        let elapsed = now.timeIntervalSince(lastFpsSample.at)
        guard elapsed >= 0.95 else { return }
        let decoded = pipeline.stats.decoder.decoded
        let delta = max(0, decoded - lastFpsSample.decoded)
        liveFps = Int((Double(delta) / elapsed).rounded())
        lastFpsSample = (decoded, now)
    }

    /// Development builds log throughput every 2 s, which tells "the network
    /// is slow" from "the decoder is behind" without a debugger.
    private func logStats() {
        #if DEBUG
        statsTicks += 1
        guard statsTicks % 4 == 0 else { return }
        let stats = pipeline.stats
        let current = (stats.received, stats.decoder.decoded, stats.decoder.skipped, stats.decoder.failed)
        Log.media.info(
            "per 2s: received \(current.0 - self.lastStats.received, privacy: .public), decoded \(current.1 - self.lastStats.decoded, privacy: .public), skipped \(current.2 - self.lastStats.skipped, privacy: .public), failed \(current.3 - self.lastStats.failed, privacy: .public)")
        lastStats = current
        #endif
    }

    /// Target landscape bounding box in pixels for the selected resolution tier.
    static func targetSize(for resolution: StreamPreferences.Resolution) -> (width: UInt32, height: UInt32) {
        if let fixed = resolution.maxSize {
            return fixed
        }
        let size = UIScreen.main.nativeBounds.size
        return (UInt32(max(size.width, size.height)), UInt32(min(size.width, size.height)))
    }

    /// The screen (or user-selected resolution cap) in landscape device pixels;
    /// streams are always landscape.
    static func viewport(resolution: StreamPreferences.Resolution = .auto, fps: UInt32) -> Wire.Viewport {
        let target = targetSize(for: resolution)
        return Wire.Viewport(
            width: target.width, height: target.height,
            dpi: UInt16(UIDevice.current.userInterfaceIdiom == .pad ? 264 : 460), fps: fps)
    }
}
