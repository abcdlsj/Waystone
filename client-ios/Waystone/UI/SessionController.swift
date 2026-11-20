import SwiftUI

/// Connection orchestration: pairing, signaling, transport and teardown.
///
/// Everything that owns state lives here so the views stay declarative and the
/// session lifecycle has exactly one owner.
@MainActor
final class SessionController: ObservableObject {
    // MARK: - Publishing

    @Published private(set) var hosts: [Host] = []
    @Published private(set) var phase: Phase = .idle
    @Published private(set) var path = PathStatus()
    @Published var lastError: String?
    @Published var selectedPreset: HotkeyPreset = .cards
    @Published var frameRate: FrameRateTier = .smooth
    @Published var touchMode: TouchMode = .direct
    @Published var audioMuted = false
    @Published private(set) var remoteSize: CGSize = .zero

    enum Phase: Equatable {
        case idle
        case pairing
        case online
        case connecting(hostName: String)
        case streaming(hostName: String)
        case ended(reason: String)

        var isStreaming: Bool {
            if case .streaming = self { return true }
            return false
        }
    }

    enum FrameRateTier: UInt32, CaseIterable {
        case smooth = 60
        case lowPower = 45

        var label: String {
            switch self {
            case .smooth: return "60 fps"
            case .lowPower: return "45 fps"
            }
        }
    }

    enum TouchMode: String, CaseIterable {
        case direct
        case trackpad

        var label: String {
            switch self {
            case .direct: return "Direct"
            case .trackpad: return "Trackpad"
            }
        }
    }

    enum HotkeyPreset: String, CaseIterable, Identifiable {
        case cards
        case visualNovel
        case routine

        var id: String { rawValue }

        var label: String {
            switch self {
            case .cards: return "Cards"
            case .visualNovel: return "Novel"
            case .routine: return "Routine"
            }
        }

        /// Capsules shown on the edge of the screen.
        var capsules: [HotkeyCapsule] {
            switch self {
            case .cards:
                return [
                    HotkeyCapsule(title: "End turn", keyCode: 49, symbol: "space"),
                    HotkeyCapsule(title: "Cancel", keyCode: 1, symbol: "rightclick"),
                    HotkeyCapsule(title: "Menu", keyCode: 53, symbol: "escape"),
                ]
            case .visualNovel:
                return [
                    HotkeyCapsule(title: "Advance", keyCode: 36, symbol: "return"),
                    HotkeyCapsule(title: "Skip", keyCode: 59, symbol: "control", toggle: true),
                ]
            case .routine:
                return [
                    HotkeyCapsule(title: "F", keyCode: 3, symbol: "hand.tap"),
                    HotkeyCapsule(title: "Jump", keyCode: 49, symbol: "arrow.up"),
                    HotkeyCapsule(title: "Map", keyCode: 46, symbol: "map"),
                ]
            }
        }
    }

    /// One edge capsule.
    struct HotkeyCapsule: Identifiable {
        var id: String { title }
        var title: String
        var keyCode: UInt32
        var symbol: String
        var toggle: Bool = false
    }

    // MARK: - Dependencies

    private let store: InstanceStore

    /// Instances known to the app; the controller keeps its own copy so it can
    /// reconnect without a view passing it in.
    var instances: [Instance] { store.instances }
    private let identity: DeviceIdentity?
    private var signaling: SignalingClient?
    private var transport: Transport?
    private var decoder: VideoDecoder?
    /// Codec the session negotiated; the decoder needs it to parse NAL units.
    private var negotiatedCodec = VideoToolboxSupport.preferredCodec()
    private var audio: AudioPlayer?
    private weak var videoSink: VideoView?

    private var currentInstance: Instance?
    private var currentSession: String?
    /// Host the user tapped before the socket finished authenticating.
    private var pendingHost: String?
    private var gesture = GestureEngine()
    #if DEBUG
    /// Frame counters for the once per second throughput log.
    private var receivedFrames = 0
    private var audioPackets = 0
    private var reportedFrames = 0
    private var reportedDecoded = 0
    private var reportedDropped = 0
    private var throughputTimer: Timer?
    #endif
    /// Relay credentials handed out by the server for this session.
    private var turn: Wire.TurnCredentials?
    /// SDP and ICE produced before the server assigned a session id.
    ///
    /// The transport starts negotiating as soon as it is created, which is before
    /// `SessionAccepted` arrives. Dropping those messages would leave the host
    /// without a remote candidate and the DTLS handshake would time out.
    private var pendingSignals: [Wire.RtcSignal] = []
    private var toggledKeys: Set<UInt32> = []
    private var stallTimer: Timer?

    init(store: InstanceStore) {
        self.store = store
        self.identity = try? DeviceIdentity(loadOrCreateFor: "primary")
    }

    // MARK: - Pairing

    /// Enrolls the instance described by a pairing URI.
    func pair(with uri: String) {
        guard var instance = Instance.from(pairingURI: uri),
            let token = Instance.token(from: uri),
            let identity
        else {
            lastError = "That pairing code is not valid."
            return
        }
        instance.pendingToken = token
        instance.label = instance.endpoint
        phase = .pairing
        lastError = nil
        currentInstance = instance

        let client = SignalingClient()
        signaling = client
        client.connect(instance: instance) { [weak self] event in
            self?.handleSignal(event)
        }
        // The challenge arrives immediately; `handleSignal` answers it with the
        // pairing request because no session is active yet.
        _ = identity
    }

    /// Reconnects to a stored instance.
    func connect(to instance: Instance) {
        currentInstance = instance
        phase = .connecting(hostName: instance.label)
        lastError = nil

        let client = SignalingClient()
        signaling = client
        client.connect(instance: instance) { [weak self] event in
            self?.handleSignal(event)
        }
    }

    func forget(_ instance: Instance) {
        store.remove(instance)
        signaling?.disconnect()
        signaling = nil
    }

    /// Connects to the stored instance, used by the development auto-start hook.
    func reconnectIfNeeded() {
        guard let instance = instances.first else { return }
        switch phase {
        case .idle:
            connect(to: instance)
        case .online:
            if let host = hosts.first(where: { $0.online }) {
                startSession(with: host)
            }
        default:
            break
        }
    }

    // MARK: - Session

    /// Connects to the instance if needed, then starts a session once authenticated.
    func connect(to instance: Instance, andStart host: Host) {
        pendingHost = host.id
        if currentInstance?.id == instance.id, phase == .online {
            startSession(with: host)
        } else {
            connect(to: instance)
        }
    }

    func startSession(with host: Host) {
        guard let identity else { return }
        phase = .connecting(hostName: host.name)

        let viewport = Wire.Viewport.current(
            screen: UIScreen.main.bounds,
            scale: UIScreen.main.scale,
            fps: frameRate.rawValue
        )

        // The simulator has no HEVC decoder at all, so the codec is chosen from
        // what this device can actually decode rather than hard coded.
        let codec = VideoToolboxSupport.preferredCodec()
        negotiatedCodec = codec
        NSLog("waystone: negotiating \(codec) video")
        let transport = Transport(
            role: "controller",
            viewport: viewport,
            audio: !audioMuted,
            stun: turn?.server ?? currentInstance?.turn,
            turn: turn.map { ($0.server, $0.username, $0.password) },
            videoCodec: codec,
            handler: { [weak self] event in
                Task { @MainActor in self?.handleTransport(event) }
            }
        )
        guard let transport else {
            fail("Cannot start the transport.")
            return
        }
        self.transport = transport

        signaling?.send(
            .sessionStart(
                target: host.id, viewport: viewport, audio: !audioMuted,
                videoCodec: codec))
        _ = identity
    }

    /// Hangs up and asks the host to lock its session.
    func hangUp(lock: Bool = true) {
        guard let session = currentSession else {
            teardown()
            return
        }
        signaling?.send(
            .sessionBye(sessionId: session, reason: lock ? "lock_and_exit" : "user_hangup"))
        teardown()
    }

    // MARK: - Options

    func setFrameRate(_ tier: FrameRateTier) {
        frameRate = tier
        guard let session = currentSession else { return }
        signaling?.send(
            .sessionControl(
                sessionId: session, control: .setFrameRate(fps: tier.rawValue)))
    }

    func requestKeyframe() {
        guard let session = currentSession else { return }
        signaling?.send(.sessionControl(sessionId: session, control: .requestKeyframe))
    }

    func setAudioMuted(_ muted: Bool) {
        audioMuted = muted
        audio?.setMuted(muted)
        guard let session = currentSession else { return }
        signaling?.send(
            .sessionControl(sessionId: session, control: .setAudioEnabled(enabled: !muted)))
    }

    func attach(view: VideoView) {
        videoSink = view
    }

    // MARK: - Input

    func touchBegan(at point: CGPoint, in size: CGSize) {
        let normalized = normalize(point, in: size)
        emit(gesture.touchDown(x: normalized.x, y: normalized.y, at: Date().timeIntervalSince1970))
    }

    func touchMoved(to point: CGPoint, in size: CGSize) {
        let normalized = normalize(point, in: size)
        emit(gesture.touchMove(x: normalized.x, y: normalized.y, at: Date().timeIntervalSince1970))
    }

    func touchEnded(at point: CGPoint, in size: CGSize) {
        let normalized = normalize(point, in: size)
        emit(gesture.touchUp(x: normalized.x, y: normalized.y, at: Date().timeIntervalSince1970))
    }

    func touchCancelled() {
        emit(gesture.cancel())
    }

    func pinch(scale: CGFloat, ended: Bool) -> CGFloat {
        gesture.pinch(scale: scale, ended: ended)
    }

    /// Sends a capsule key press, toggling the key when the capsule asks for it.
    func press(_ capsule: HotkeyCapsule) {
        guard let transport else { return }
        if capsule.toggle {
            if toggledKeys.contains(capsule.keyCode) {
                toggledKeys.remove(capsule.keyCode)
                transport.sendInput(InputFrame.key(code: capsule.keyCode, down: false, modifiers: 0), reliable: true)
            } else {
                toggledKeys.insert(capsule.keyCode)
                transport.sendInput(InputFrame.key(code: capsule.keyCode, down: true, modifiers: 0), reliable: true)
            }
            return
        }
        transport.sendInput(InputFrame.key(code: capsule.keyCode, down: true, modifiers: 0), reliable: true)
        transport.sendInput(InputFrame.key(code: capsule.keyCode, down: false, modifiers: 0), reliable: true)
    }

    // MARK: - Signaling

    private func handleSignal(_ event: SignalingClient.Event) {
        switch event {
        case .stateChanged(let state):
            if case .failed(let reason) = state {
                fail(reason)
            }
        case .closed(let reason):
            fail(reason)
        case .message(let message):
            handle(message)
        }
    }

    private func handle(_ message: Wire.ServerMessage) {
        switch message {
        case .challenge(let nonce, let serverId, _):
            guard let identity, let instance = currentInstance else { return }
            let challenge = Wire.Challenge(nonce: nonce, serverId: serverId)
            let signature = identity.sign(challenge.signingPayload())
            if let token = instance.pendingToken {
                signaling?.send(
                    .pairRequest(
                        token: token,
                        device: identity.info(name: deviceName(), platform: platform())))
                // The pairing request does not carry the signature; the server
                // answers with PairAccepted and the flow continues below.
                _ = signature
            } else {
                signaling?.send(
                    .auth(
                        identityId: identity.id,
                        publicKey: identity.publicKeyBase64,
                        signature: signature,
                        name: deviceName()))
            }

        case .pairAccepted(let serverId):
            guard let identity, var instance = currentInstance else { return }
            instance.serverId = serverId
            instance.pendingToken = nil
            instance.lastSeen = Date()
            currentInstance = instance
            store.upsert(instance)
            phase = .online
            // Authenticate immediately; the server issues a new challenge.
            signaling?.disconnect()
            connect(to: instance)
            _ = identity

        case .pairRejected(let reason):
            fail(reason)

        case .authOk(let peers, let turn):
            self.turn = turn
            hosts = peers.filter { $0.device.role == "host" }.map(Host.init)
            phase = .online
            if let instance = currentInstance {
                store.markSeen(instance)
            }
            if let hostId = pendingHost, let host = hosts.first(where: { $0.id == hostId }) {
                pendingHost = nil
                startSession(with: host)
            }

        case .authRejected(let reason):
            fail(reason)

        case .peerUpdate(let peers):
            hosts = peers.filter { $0.device.role == "host" }.map(Host.init)

        case .sessionAccepted(let sessionId, _, let peer, _):
            currentSession = sessionId
            // The media graph can only be built once the server assigned a
            // session id, because every ICE candidate is relayed with it.
            beginStream(hostName: peer.name)
            for signal in pendingSignals {
                signaling?.send(.rtcSignal(sessionId: sessionId, signal: signal))
            }
            pendingSignals.removeAll()

        case .sessionRejected(let reason):
            fail(reason)

        case .rtcSignal(_, let signal):
            switch signal {
            case .answer(let sdp):
                transport?.acceptAnswer(sdp)
            case .offer(let sdp):
                transport?.acceptOffer(sdp)
            case .ice(let candidate):
                transport?.addCandidate(candidate)
            case .iceComplete:
                break
            }

        case .sessionControl:
            break

        case .sessionEnded(_, let reason, let detail):
            NSLog("waystone: session ended: \(reason) \(detail ?? "")")
            teardown()
            phase = .ended(reason: detail ?? reason)
            lastError = detail ?? reason

        case .pong:
            break

        case .error(let message):
            lastError = message
        }
    }

    // MARK: - Transport

    private func beginStream(hostName: String) {
        guard let transport else { return }
        decoder = VideoDecoder(codec: DecodeCodec(name: negotiatedCodec))
        decoder?.onFrame = { [weak self] buffer in
            guard let self, let sink = self.videoSink else { return }
            let size = CGSize(
                width: CVPixelBufferGetWidth(buffer),
                height: CVPixelBufferGetHeight(buffer))
            DispatchQueue.main.async {
                self.remoteSize = size
                sink.present(buffer, videoSize: size)
            }
        }
        decoder?.onFormatChange = { [weak self] size in
            Task { @MainActor in self?.remoteSize = size }
        }

        let audio = AudioPlayer()
        if !audioMuted {
            try? audio.start()
        }
        self.audio = audio

        phase = .streaming(hostName: hostName)
        startStallWatchdog()
        #if DEBUG
        startThroughputLog()
        #endif
    }

    private func handleTransport(_ event: Transport.Event) {
        switch event {
        case .localOffer(let sdp):
            sendOrBuffer(.offer(sdp: sdp))
        case .localAnswer(let sdp):
            sendOrBuffer(.answer(sdp: sdp))
        case .localCandidate(let candidate):
            sendOrBuffer(.ice(candidate: candidate))
        case .connected:
            NSLog("waystone: transport connected")
            path.path = .direct
        case .disconnected:
            NSLog("waystone: transport disconnected")
            path.path = .unknown
        case .mediaReady:
            NSLog("waystone: media negotiated")
        case .pathUpdate(let rtt, let relayed, let bitrate):
            path = PathStatus(path: relayed ? .relayed : .direct, rttMs: rtt, estimatedBitrate: bitrate)
        case .keyframeRequest:
            requestKeyframe()
        case .closed(let reason):
            fail(reason)
        case .video(let data, let keyframe, let rtpTime):
            if keyframe {
                decoder?.reset()
            }
            #if DEBUG
            receivedFrames += 1
            if receivedFrames == 1 {
                NSLog("waystone: first video frame (\(data.count) bytes, keyframe=\(keyframe))")
            }
            #endif
            decoder?.decode(annexB: data, rtpTime: rtpTime)
        case .audio(let data, _):
            #if DEBUG
            audioPackets += 1
            if audioPackets == 1 {
                NSLog("waystone: first audio packet (\(data.count) bytes)")
            }
            #endif
            audio?.enqueue(opus: data)
        case .input:
            break
        }
    }

    // MARK: - Helpers

    /// Relays a signaling message, or holds it until the session id exists.
    private func sendOrBuffer(_ signal: Wire.RtcSignal) {
        if let session = currentSession {
            signaling?.send(.rtcSignal(sessionId: session, signal: signal))
        } else {
            pendingSignals.append(signal)
        }
    }

    private func emit(_ outputs: [GestureEngine.Output]) {
        guard let transport else { return }
        for output in outputs {
            switch output {
            case .none:
                break
            case .move(let x, let y):
                transport.sendInput(InputFrame.mouseMove(x: x, y: y), reliable: false)
            case .leftDown(let x, let y):
                transport.sendInput(
                    InputFrame.mouseButton(.left, down: true, x: x, y: y), reliable: true)
            case .leftUp(let x, let y):
                transport.sendInput(
                    InputFrame.mouseButton(.left, down: false, x: x, y: y), reliable: true)
            case .rightClick(let x, let y):
                transport.sendInput(
                    InputFrame.mouseButton(.right, down: true, x: x, y: y), reliable: true)
                transport.sendInput(
                    InputFrame.mouseButton(.right, down: false, x: x, y: y), reliable: true)
            case .wheel(let dx, let dy):
                transport.sendInput(InputFrame.wheel(dx: dx, dy: dy), reliable: true)
            case .haptic(let kind):
                Haptics.play(kind)
            }
        }
    }

    private func normalize(_ point: CGPoint, in size: CGSize) -> (x: Double, y: Double) {
        guard size.width > 0, size.height > 0 else { return (0.5, 0.5) }
        return (
            min(max(Double(point.x / size.width), 0), 1),
            min(max(Double(point.y / size.height), 0), 1)
        )
    }

    private func deviceName() -> String {
        UIDevice.current.name
    }

    private func platform() -> String {
        UIDevice.current.userInterfaceIdiom == .pad ? "ipados" : "ios"
    }

    private func fail(_ reason: String) {
        pendingHost = nil
        NSLog("waystone: session failed: \(reason)")
        lastError = reason
        phase = .ended(reason: reason)
        teardown()
    }

    /// Stops everything and releases the decoder.
    private func teardown() {
        stallTimer?.invalidate()
        stallTimer = nil
        #if DEBUG
        throughputTimer?.invalidate()
        throughputTimer = nil
        #endif
        // Releasing held keys prevents a stuck Ctrl on the host.
        transport?.sendInput(InputFrame.releaseAll(), reliable: true)
        transport?.stop()
        transport = nil
        decoder = nil
        audio?.stop()
        audio = nil
        currentSession = nil
        pendingSignals.removeAll()
        toggledKeys.removeAll()
        gesture = GestureEngine()
        path = PathStatus()
        videoSink?.clear()
    }

    #if DEBUG
    /// Logs how many frames arrive, decode and get skipped every second.
    ///
    /// This is the only way to tell "the network is slow" from "the decoder is
    /// behind" from "the renderer cannot keep up" on a device.
    private func startThroughputLog() {
        throughputTimer?.invalidate()
        reportedFrames = 0
        reportedDecoded = 0
        reportedDropped = 0
        throughputTimer = Timer.scheduledTimer(withTimeInterval: 1, repeats: true) { [weak self] _ in
            guard let self else { return }
            let decoded = self.decoder?.decodedCount ?? 0
            let dropped = self.decoder?.droppedCount ?? 0
            NSLog(
                "waystone: rx +\(self.receivedFrames - self.reportedFrames)/s "
                    + "decoded +\(decoded - self.reportedDecoded)/s "
                    + "dropped +\(dropped - self.reportedDropped)/s")
            self.reportedFrames = self.receivedFrames
            self.reportedDecoded = decoded
            self.reportedDropped = dropped
        }
    }
    #endif

    /// Shows the recovery banner when frames stop for three seconds.
    private func startStallWatchdog() {
        stallTimer?.invalidate()
        stallTimer = Timer.scheduledTimer(withTimeInterval: 3, repeats: false) { [weak self] _ in
            Task { @MainActor in
                guard let self, self.phase.isStreaming, self.remoteSize == .zero else { return }
                self.lastError = "Waiting for the first frame…"
            }
        }
    }
}

/// Small wrapper so haptics are easy to stub in tests.
enum Haptics {
    static func play(_ kind: GestureEngine.Haptic) {
        switch kind {
        case .grab:
            let generator = UIImpactFeedbackGenerator(style: .light)
            generator.prepare()
            generator.impactOccurred()
        case .confirm:
            let generator = UIImpactFeedbackGenerator(style: .medium)
            generator.prepare()
            generator.impactOccurred()
        }
    }
}
