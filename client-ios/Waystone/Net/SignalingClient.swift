import CryptoKit
import Foundation

/// WSS client for the signaling channel.
///
/// `URLSessionWebSocketTask` is used instead of a third party library so the app
/// has no networking dependencies. Certificate pinning happens in the session
/// delegate: the server uses a self-signed certificate whose SHA-256 fingerprint
/// was carried by the pairing QR code.
@MainActor
final class SignalingClient: NSObject {
    enum State: Equatable {
        case idle
        case connecting
        case ready
        case failed(String)
    }

    /// Events surfaced to the view model.
    enum Event {
        case message(Wire.ServerMessage)
        case stateChanged(State)
        case closed(String)
    }

    private var session: URLSession!
    private var task: URLSessionWebSocketTask?
    private let queue = DispatchQueue(label: "dev.waystone.signaling")
    private var handler: ((Event) -> Void)?
    private(set) var state: State = .idle
    private var fingerprint: String = ""
    private var pingTimer: Timer?

    override init() {
        super.init()
    }

    /// Opens the socket and starts delivering events to `handler`.
    func connect(instance: Instance, handler: @escaping (Event) -> Void) {
        disconnect()
        self.handler = handler
        self.fingerprint = instance.fingerprint.lowercased()

        let configuration = URLSessionConfiguration.default
        configuration.waitsForConnectivity = true
        configuration.timeoutIntervalForRequest = 15
        session = URLSession(
            configuration: configuration,
            delegate: PinningDelegate(expectedFingerprint: fingerprint, host: instance.endpoint),
            delegateQueue: nil
        )

        guard let url = URL(string: "wss://\(instance.endpoint)/ws") else {
            publish(.stateChanged(.failed("invalid endpoint")))
            return
        }

        publish(.stateChanged(.connecting))
        let socket = session.webSocketTask(with: url)
        // Keep the socket alive while the app is backgrounded briefly.
        socket.maximumMessageSize = 4 * 1024 * 1024
        task = socket
        socket.resume()
        receive()
        startPing()
    }

    func disconnect() {
        pingTimer?.invalidate()
        pingTimer = nil
        task?.cancel(with: .goingAway, reason: nil)
        task = nil
        session?.invalidateAndCancel()
        session = nil
        publish(.stateChanged(.idle))
    }

    func send(_ message: Wire.ClientMessage) {
        guard let task else { return }
        let text = message.encoded()
        task.send(.string(text)) { [weak self] error in
            guard let error else { return }
            Task { @MainActor in
                self?.publish(.closed("send failed: \(error.localizedDescription)"))
            }
        }
    }

    private func receive() {
        task?.receive { [weak self] result in
            guard let self else { return }
            switch result {
            case .failure(let error):
                Task { @MainActor in
                    self.publish(.closed(error.localizedDescription))
                }
            case .success(let message):
                switch message {
                case .string(let text):
                    Task { @MainActor in
                        if let decoded = Wire.ServerMessage.decode(text) {
                            self.publish(.message(decoded))
                        }
                    }
                case .data(let data):
                    if let text = String(data: data, encoding: .utf8),
                        let decoded = Wire.ServerMessage.decode(text) {
                        Task { @MainActor in
                            self.publish(.message(decoded))
                        }
                    }
                @unknown default:
                    break
                }
                Task { @MainActor in
                    self.receive()
                }
            }
        }
    }

    private func startPing() {
        // A 20 s ping keeps NAT mappings alive and detects a dead socket before
        // the user notices a frozen frame.
        pingTimer = Timer.scheduledTimer(withTimeInterval: 20, repeats: true) { [weak self] _ in
            guard let self else { return }
            self.task?.sendPing { error in
                guard let error else { return }
                Task { @MainActor in
                    self.publish(.closed("ping failed: \(error.localizedDescription)"))
                }
            }
        }
    }

    private func publish(_ event: Event) {
        if case .stateChanged(let newState) = event {
            state = newState
        }
        handler?(event)
    }
}

/// Pins the server certificate by SHA-256 fingerprint.
///
/// Hostname verification is skipped on purpose: a self-hosted instance usually has
/// no DNS name, and the fingerprint is a stronger check than a CA chain.
final class PinningDelegate: NSObject, URLSessionDelegate {
    private let expectedFingerprint: String
    private let host: String

    init(expectedFingerprint: String, host: String) {
        self.expectedFingerprint = expectedFingerprint
        self.host = host
    }

    func urlSession(
        _ session: URLSession,
        didReceive challenge: URLAuthenticationChallenge,
        completionHandler: @escaping (URLSession.AuthChallengeDisposition, URLCredential?) -> Void
    ) {
        guard challenge.protectionSpace.authenticationMethod == NSURLAuthenticationMethodServerTrust,
            let trust = challenge.protectionSpace.serverTrust
        else {
            completionHandler(.performDefaultHandling, nil)
            return
        }

        guard let certificate = certificate(from: trust) else {
            completionHandler(.cancelAuthenticationChallenge, nil)
            return
        }

        let digest = SHA256Digest.hex(certificate)
        if digest == expectedFingerprint {
            completionHandler(.useCredential, URLCredential(trust: trust))
        } else {
            NSLog("waystone: certificate pin mismatch, refusing the connection")
            completionHandler(.cancelAuthenticationChallenge, nil)
        }
    }

    private func certificate(from trust: SecTrust) -> Data? {
        if #available(iOS 15.0, *) {
            guard let chain = SecTrustCopyCertificateChain(trust) as? [SecCertificate],
                let first = chain.first
            else { return nil }
            return SecCertificateCopyData(first) as Data
        }
        guard let certificate = SecTrustGetCertificateAtIndex(trust, 0) else { return nil }
        return SecCertificateCopyData(certificate) as Data
    }
}

/// Small helper so the delegate does not need to import CryptoKit in two places.
enum SHA256Digest {
    static func hex(_ data: Data) -> String {
        CryptoKit.SHA256.hash(data: data).map { String(format: "%02x", $0) }.joined()
    }
}
