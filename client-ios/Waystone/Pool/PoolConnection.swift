import Foundation
import UIKit

/// A machine in the private pool that can be streamed.
struct Host: Identifiable, Equatable {
    var id: String
    var name: String
    var platform: Wire.Platform
    var online: Bool
    var busy: Bool
    /// Server-to-host round trip, a hint of how far away the machine is.
    var rttMs: UInt32?

    init(_ peer: Wire.PeerState) {
        id = peer.device.id
        name = peer.device.name
        platform = peer.device.platform
        online = peer.online
        busy = peer.busy
        rttMs = peer.rttMs
    }
}

/// What a stream session needs from the signaling layer.
@MainActor
protocol SessionSignaling: AnyObject {
    /// True while the server connection is authenticated.
    var isOnline: Bool { get }
    /// The current state of a host, from the latest peer list.
    func host(id: String) -> Host?
    func send(_ message: Wire.ClientMessage)
}

/// Receives the messages of one session.
@MainActor
protocol SessionMessageHandler: AnyObject {
    func sessionAccepted(id: String, codec: Wire.VideoCodec)
    func sessionRejected(reason: String)
    func received(_ signal: Wire.RtcSignal)
    func received(_ control: Wire.SessionControl)
    func sessionEnded(reason: Wire.ByeReason, detail: String?)
    /// The signaling connection dropped; the media path may survive it.
    func signalingLost()
}

/// The long-lived, authenticated connection to one server.
///
/// It keeps the host list fresh, reconnects with backoff, and routes session
/// messages to the session that owns them. It also performs pairing: a link's
/// token is spent on the same socket that then authenticates.
@MainActor
final class PoolConnection: ObservableObject, Identifiable, SessionSignaling {
    enum Status: Equatable {
        case connecting
        case online
        /// Temporarily unreachable; retries on its own.
        case offline(reason: String)
        /// The server no longer accepts this device. Retrying cannot help.
        case rejected(reason: String)
    }

    @Published private(set) var status: Status = .connecting
    @Published private(set) var hosts: [Host] = []

    let instance: Instance
    var id: Instance.ID { instance.id }
    var isOnline: Bool { status == .online }

    /// Called once when a pairing token was accepted.
    var onPaired: (() -> Void)?
    /// Called when pairing fails, with a message for the user.
    var onPairingFailed: ((String) -> Void)?

    /// Relay credentials for sessions started on this connection.
    private(set) var turn: Wire.TurnCredentials?

    private let identity: DeviceIdentity
    private var pairingToken: String?
    private var client: SignalingClient?
    private var challenge: (nonce: String, serverId: String)?
    private var retryTask: Task<Void, Never>?
    private var failures = 0
    private var running = false

    private weak var session: SessionMessageHandler?
    private var sessionId: String?

    init(instance: Instance, identity: DeviceIdentity, pairingToken: String? = nil) {
        self.instance = instance
        self.identity = identity
        self.pairingToken = pairingToken
    }

    // MARK: - Lifecycle

    func start() {
        guard !running else { return }
        running = true
        failures = 0
        connect()
    }

    /// Closes the socket; used when the app goes to the background.
    func stop() {
        running = false
        retryTask?.cancel()
        retryTask = nil
        client?.close()
        client = nil
        if case .rejected = status { return }
        status = .offline(reason: "已暂停")
    }

    /// Skips the backoff, for pull to refresh and returning to the app.
    func reconnectNow() {
        guard running else { return start() }
        guard status != .online, client == nil || status != .connecting else { return }
        retryTask?.cancel()
        failures = 0
        connect()
    }

    // MARK: - Sessions

    /// Asks the server for a session; replies go to `handler`.
    func requestSession(
        with host: Host, viewport: Wire.Viewport, audio: Bool, codec: Wire.VideoCodec,
        handler: SessionMessageHandler
    ) {
        session = handler
        sessionId = nil
        send(.sessionStart(target: host.id, viewport: viewport, audio: audio, videoCodec: codec))
    }

    /// Forgets the session once it is over, so late messages are dropped.
    func releaseSession(_ handler: SessionMessageHandler) {
        guard session === handler else { return }
        session = nil
        sessionId = nil
    }

    func host(id: String) -> Host? {
        hosts.first { $0.id == id }
    }

    func send(_ message: Wire.ClientMessage) {
        client?.send(message)
    }

    // MARK: - Connection

    private func connect() {
        client?.close()
        challenge = nil
        status = .connecting
        let client: SignalingClient
        do {
            client = try SignalingClient(endpoint: instance.endpoint, fingerprint: instance.fingerprint)
        } catch {
            fail("服务器地址无效")
            return
        }
        self.client = client
        client.start { [weak self, weak client] event in
            guard let self, let client, client === self.client else { return }
            self.handle(event)
        }
    }

    private func handle(_ event: SignalingClient.Event) {
        switch event {
        case .opened:
            break
        case .message(let message):
            handle(message)
        case .closed(let reason):
            Log.signal.info("connection to \(self.instance.endpoint) closed: \(reason)")
            client = nil
            session?.signalingLost()
            if pairingToken != nil {
                pairingToken = nil
                onPairingFailed?("无法连接到服务器：\(reason)")
                running = false
                status = .offline(reason: reason)
                return
            }
            if case .rejected = status { return }
            fail(Self.describe(reason))
        }
    }

    private func handle(_ message: Wire.ServerMessage) {
        switch message {
        case .challenge(let nonce, let serverId):
            challenge = (nonce, serverId)
            if let token = pairingToken {
                send(.pairRequest(token: token, device: identity.info(name: Self.deviceName, platform: Self.platform)))
            } else {
                authenticate()
            }

        case .pairAccepted:
            pairingToken = nil
            onPaired?()
            // The challenge of this connection is still unused, so the same
            // socket authenticates straight away.
            authenticate()

        case .pairRejected(let reason):
            pairingToken = nil
            running = false
            status = .rejected(reason: reason)
            onPairingFailed?(Self.describePairing(reason))
            client?.close()
            client = nil

        case .authOk(let peers, let turn):
            failures = 0
            self.turn = turn
            apply(peers)
            status = .online

        case .authRejected(let reason):
            client?.close()
            client = nil
            if reason.contains("replaced") {
                // Another connection from this phone won; it is not an error.
                fail("已在其他地方连接")
            } else {
                running = false
                status = .rejected(reason: Self.describeRejection(reason))
            }

        case .peerUpdate(let peers):
            apply(peers)

        case .sessionAccepted(let id, _, let codec):
            sessionId = id
            session?.sessionAccepted(id: id, codec: codec)

        case .sessionRejected(let reason):
            session?.sessionRejected(reason: reason)

        case .rtcSignal(let id, let signal):
            guard id == sessionId else { return }
            session?.received(signal)

        case .sessionControl(let id, let control):
            guard id == sessionId else { return }
            session?.received(control)

        case .sessionEnded(let id, let reason, let detail):
            guard id == sessionId else { return }
            session?.sessionEnded(reason: reason, detail: detail)

        case .pong:
            break

        case .error(let message):
            Log.signal.error("server error: \(message)")
        }
    }

    private func authenticate() {
        guard let challenge else { return }
        do {
            let payload = Wire.signingPayload(serverId: challenge.serverId, nonce: challenge.nonce)
            let signature = try identity.sign(payload)
            send(.auth(identity: identity.wireIdentity, signature: signature, name: Self.deviceName))
        } catch {
            fail("无法签名身份：\(error.localizedDescription)")
        }
        self.challenge = nil
    }

    private func apply(_ peers: [Wire.PeerState]) {
        hosts = peers.filter { $0.device.role == .host }.map(Host.init)
            .sorted { ($0.online ? 0 : 1, $0.name) < ($1.online ? 0 : 1, $1.name) }
    }

    /// Marks the server offline and schedules a retry with capped backoff.
    private func fail(_ reason: String) {
        client?.close()
        client = nil
        status = .offline(reason: reason)
        for index in hosts.indices { hosts[index].online = false }
        guard running else { return }
        failures += 1
        let delay = min(pow(2, Double(failures - 1)), 30)
        retryTask?.cancel()
        retryTask = Task { [weak self] in
            try? await Task.sleep(for: .seconds(delay))
            guard let self, !Task.isCancelled, self.running else { return }
            self.connect()
        }
    }

    // MARK: - Wording

    private static func describe(_ reason: String) -> String {
        let lowered = reason.lowercased()
        if lowered.contains("cancel") || lowered.contains("certificate") {
            return "证书校验失败，请重新扫码添加"
        }
        if lowered.contains("timed out") || lowered.contains("超时") {
            return "连接服务器超时"
        }
        if lowered.contains("offline") || lowered.contains("internet") {
            return "手机没有网络"
        }
        return "无法连接服务器"
    }

    private static func describePairing(_ reason: String) -> String {
        if reason.contains("expired") || reason.contains("used") || reason.contains("invalid") {
            return "配对码已失效，请在面板上重新生成"
        }
        return "配对失败：\(reason)"
    }

    private static func describeRejection(_ reason: String) -> String {
        if reason.contains("not paired") || reason.contains("revoked") {
            return "此手机已被移出设备池，请重新扫码添加"
        }
        return reason
    }

    static var deviceName: String { UIDevice.current.name }

    static var platform: Wire.Platform {
        UIDevice.current.userInterfaceIdiom == .pad ? .ipados : .ios
    }
}
