import CryptoKit
import Foundation

/// One WSS connection to a Waystone server.
///
/// The server uses a self-signed certificate, so trust comes from the SHA-256
/// fingerprint carried by the pairing link instead of a CA chain. A client is
/// single use: once it reports `closed`, create a new one.
@MainActor
final class SignalingClient {
    enum Event {
        case opened
        case message(Wire.ServerMessage)
        case closed(reason: String)
    }

    private let task: URLSessionWebSocketTask
    private let session: URLSession
    private var onEvent: ((Event) -> Void)?
    private var closed = false

    init(endpoint: String, fingerprint: String) throws {
        guard let url = URL(string: "wss://\(endpoint)/ws") else {
            throw URLError(.badURL)
        }
        let configuration = URLSessionConfiguration.ephemeral
        configuration.timeoutIntervalForRequest = 10
        configuration.waitsForConnectivity = false
        let delegate = SocketDelegate(fingerprint: fingerprint)
        session = URLSession(configuration: configuration, delegate: delegate, delegateQueue: nil)
        task = session.webSocketTask(with: url)
        task.maximumMessageSize = 1 << 20
        delegate.client = self
    }

    func start(onEvent: @escaping (Event) -> Void) {
        self.onEvent = onEvent
        task.resume()
        receive()
    }

    func send(_ message: Wire.ClientMessage) {
        guard !closed else { return }
        task.send(.string(message.json())) { [weak self] error in
            guard let error else { return }
            Task { @MainActor in self?.finish("send failed: \(error.localizedDescription)") }
        }
    }

    func close() {
        guard !closed else { return }
        closed = true
        onEvent = nil
        task.cancel(with: .normalClosure, reason: nil)
        session.invalidateAndCancel()
    }

    // MARK: - Private

    private func receive() {
        task.receive { [weak self] result in
            Task { @MainActor in
                guard let self, !self.closed else { return }
                switch result {
                case .failure(let error):
                    self.finish(error.localizedDescription)
                case .success(let frame):
                    let text: String?
                    switch frame {
                    case .string(let value): text = value
                    case .data(let data): text = String(data: data, encoding: .utf8)
                    @unknown default: text = nil
                    }
                    if let text {
                        do {
                            self.onEvent?(.message(try Wire.ServerMessage.decode(text)))
                        } catch {
                            // An unknown message from a newer server is not fatal.
                            Log.signal.info("ignoring message: \(error.localizedDescription, privacy: .public)")
                        }
                    }
                    self.receive()
                }
            }
        }
    }

    fileprivate func didOpen() {
        guard !closed else { return }
        onEvent?(.opened)
    }

    fileprivate func finish(_ reason: String) {
        guard !closed else { return }
        let handler = onEvent
        close()
        handler?(.closed(reason: reason))
    }
}

/// Pins the certificate and reports socket lifecycle back to the client.
private final class SocketDelegate: NSObject, URLSessionWebSocketDelegate {
    private let fingerprint: String
    weak var client: SignalingClient?

    init(fingerprint: String) {
        self.fingerprint = fingerprint.lowercased()
    }

    func urlSession(
        _ session: URLSession,
        didReceive challenge: URLAuthenticationChallenge,
        completionHandler: @escaping (URLSession.AuthChallengeDisposition, URLCredential?) -> Void
    ) {
        guard challenge.protectionSpace.authenticationMethod == NSURLAuthenticationMethodServerTrust,
            let trust = challenge.protectionSpace.serverTrust,
            let leaf = (SecTrustCopyCertificateChain(trust) as? [SecCertificate])?.first
        else {
            completionHandler(.performDefaultHandling, nil)
            return
        }
        let digest = SHA256.hash(data: SecCertificateCopyData(leaf) as Data)
            .map { String(format: "%02x", $0) }.joined()
        if digest == fingerprint {
            completionHandler(.useCredential, URLCredential(trust: trust))
        } else {
            Log.signal.error("certificate fingerprint mismatch")
            completionHandler(.cancelAuthenticationChallenge, nil)
        }
    }

    func urlSession(
        _ session: URLSession, webSocketTask: URLSessionWebSocketTask,
        didOpenWithProtocol protocol: String?
    ) {
        Task { @MainActor [weak client] in client?.didOpen() }
    }

    func urlSession(
        _ session: URLSession, webSocketTask: URLSessionWebSocketTask,
        didCloseWith closeCode: URLSessionWebSocketTask.CloseCode, reason: Data?
    ) {
        let text = reason.flatMap { String(data: $0, encoding: .utf8) } ?? "closed (\(closeCode.rawValue))"
        Task { @MainActor [weak client] in client?.finish(text) }
    }

    func urlSession(_ session: URLSession, task: URLSessionTask, didCompleteWithError error: Error?) {
        guard let error else { return }
        Task { @MainActor [weak client] in client?.finish(error.localizedDescription) }
    }
}
