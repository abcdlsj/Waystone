import Foundation
import SwiftUI

/// The root of the app: enrolled servers, their live connections, pairing and
/// the one stream that may be running.
@MainActor
final class AppModel: ObservableObject {
    enum Pairing: Equatable {
        case idle
        case inProgress(endpoint: String)
        case failed(String)
    }

    @Published private(set) var pools: [PoolConnection] = []
    @Published private(set) var session: StreamSession?
    @Published private(set) var pairing: Pairing = .idle
    /// A one-line notice for the home screen: why the last stream ended, or
    /// that a server was added.
    @Published var notice: Notice?

    struct Notice: Identifiable, Equatable {
        let id = UUID()
        var title: String
        var message: String?
    }

    let preferences = StreamPreferences()
    private let store: InstanceStore
    private let identity: DeviceIdentity?
    private var sessionObservation: Task<Void, Never>?

    init(store: InstanceStore? = nil) {
        let store = store ?? InstanceStore()
        self.store = store
        do {
            identity = try DeviceIdentity(loadOrCreateFor: "primary")
        } catch {
            identity = nil
            Log.app.error("cannot load the device identity: \(error.localizedDescription, privacy: .public)")
        }
        if let identity {
            pools = store.instances.map { PoolConnection(instance: $0, identity: identity) }
        }
    }

    var identityError: Bool { identity == nil }

    // MARK: - App lifecycle

    /// Connections only live while the app is visible; a running stream keeps
    /// its own connection alive.
    func scenePhaseChanged(_ phase: ScenePhase) {
        switch phase {
        case .active:
            // `start` is a no-op for running connections; `reconnectNow`
            // skips the backoff of ones that are waiting to retry.
            pools.forEach { $0.start() }
            pools.forEach { $0.reconnectNow() }
        case .background:
            guard session == nil else { return }
            pools.forEach { $0.stop() }
        default:
            break
        }
    }

    func refresh() {
        pools.forEach { $0.reconnectNow() }
    }

    // MARK: - Pairing

    /// Enrolls the server behind a scanned, tapped or pasted pairing link.
    func pair(with text: String) {
        guard let identity else {
            pairing = .failed("无法读取本机身份，请重新安装 App")
            return
        }
        guard let link = PairingLink(text) else {
            pairing = .failed("这不是 Waystone 配对码")
            return
        }
        let instance = Instance(link: link)
        if let existing = pools.firstIndex(where: { $0.id == instance.id }) {
            pools[existing].stop()
            pools.remove(at: existing)
        }
        let pool = PoolConnection(instance: instance, identity: identity, pairingToken: link.token)
        pairing = .inProgress(endpoint: link.endpoint)
        pool.onPaired = { [weak self, weak pool] in
            guard let self, let pool else { return }
            self.store.upsert(pool.instance)
            self.pools.append(pool)
            self.pairing = .idle
            self.pendingPairing = nil
            self.notice = Notice(title: "已添加 \(pool.instance.label)", message: nil)
        }
        pool.onPairingFailed = { [weak self] reason in
            self?.pairing = .failed(reason)
            self?.pendingPairing = nil
        }
        pool.start()
        pendingPairing = pool
    }

    /// Keeps the pairing connection alive until it reports back.
    private var pendingPairing: PoolConnection?

    func dismissPairingError() {
        if case .failed = pairing { pairing = .idle }
    }

    func forget(_ pool: PoolConnection) {
        pool.stop()
        pools.removeAll { $0.id == pool.id }
        store.remove(id: pool.id)
    }

    // MARK: - Streaming

    func connect(to host: Host, in pool: PoolConnection) {
        guard session == nil, host.online else { return }
        let session = StreamSession(host: host, pool: pool, preferences: preferences)
        self.session = session
        session.start()
        sessionObservation = Task { [weak self, weak session] in
            guard let session else { return }
            for await stage in session.$stage.values {
                guard case .ended(let ending) = stage else { continue }
                self?.sessionEnded(ending)
                break
            }
        }
    }

    private func sessionEnded(_ ending: StreamSession.Ending) {
        session = nil
        sessionObservation = nil
        if ending != .hungUp {
            notice = Notice(title: ending.title, message: ending.message)
        }
    }

    // MARK: - Development

    #if DEBUG
    /// Starts a stream with the first online host, for unattended runs.
    /// Streams the first free host, or the first whose name contains
    /// `named` (case-insensitive) when given.
    func autoConnect(named: String? = nil) {
        guard session == nil else { return }
        for pool in pools where pool.isOnline {
            let match = { (host: Host) in
                named.map { host.name.localizedCaseInsensitiveContains($0) } ?? true
            }
            if let host = pool.hosts.first(where: { $0.online && !$0.busy && match($0) }) {
                connect(to: host, in: pool)
                return
            }
        }
    }
    #endif
}
