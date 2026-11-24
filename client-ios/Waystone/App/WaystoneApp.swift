import SwiftUI

@main
struct WaystoneApp: App {
    @StateObject private var store: InstanceStore
    @StateObject private var controller: SessionController

    init() {
        let store = InstanceStore()
        _store = StateObject(wrappedValue: store)
        _controller = StateObject(wrappedValue: SessionController(store: store))
    }

    var body: some Scene {
        WindowGroup {
            HomeView()
                .environmentObject(store)
                .environmentObject(controller)
                .preferredColorScheme(.dark)
                .statusBarHidden(controller.phase.isStreaming)
                .onOpenURL { url in
                    // The pairing URI is a `waystone://` URL, so the Camera app
                    // and Safari can hand it straight to the app.
                    guard url.absoluteString.hasPrefix("waystone://pair") else { return }
                    controller.pair(with: url.absoluteString)
                }
                .task {

                    #if DEBUG
                    // Development affordance: a simulator cannot scan a QR code,
                    // so the pairing URI can be injected with
                    // `simctl launch --setenv SIMCTL_CHILD_WAYSTONE_PAIR_URI=...`
                    // On a device, `devicectl device copy to` drops it into
                    // Documents/waystone-pair.txt instead. Both are ignored in
                    // release builds.
                    let inbox = PairingInbox.take()
                    let injected =
                        ProcessInfo.processInfo.environment["WAYSTONE_PAIR_URI"] ?? inbox?.uri
                    if let injected, store.instances.isEmpty {
                        NSLog("waystone: pairing from the injected URI")
                        controller.pair(with: injected)
                    }
                    // After pairing, optionally auto-connect to the first online
                    // host so a headless run can exercise the media path.
                    let autoConnect =
                        ProcessInfo.processInfo.environment["WAYSTONE_AUTO_CONNECT"] != nil
                        || inbox?.autoConnect == true
                    if autoConnect {
                        // Reconnect in a loop: the hook is only for development,
                        // and the state machine drives what happens next.
                        while !Task.isCancelled {
                            controller.reconnectIfNeeded()
                            try? await Task.sleep(nanoseconds: 1_500_000_000)
                        }
                    }
                    #endif
                }
        }
    }
}

#if DEBUG
/// Reads a pairing URI from `Documents/waystone-pair.txt`.
///
/// This is the device counterpart of the simulator's environment variable: a
/// real phone cannot be told to scan a QR code by a script, but
/// `xcrun devicectl device copy to --domain-type appDataContainer` can drop a
/// file into the app's container. The file is consumed once and deleted.
struct PairingInbox {
    let uri: String
    /// `true` when a second line asks the app to start the session too.
    let autoConnect: Bool

    static func take() -> PairingInbox? {
        let manager = FileManager.default
        let support = manager.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
        // `devicectl` can only write into a directory that already exists, and the
        // app support folder is the one it creates for itself.
        let candidates = [
            support.appendingPathComponent("Waystone/pair.txt"),
            manager.urls(for: .documentDirectory, in: .userDomainMask)[0]
                .appendingPathComponent("waystone-pair.txt"),
        ]
        guard
            let file = candidates.first(where: { manager.fileExists(atPath: $0.path) }),
            let text = try? String(contentsOf: file, encoding: .utf8)
        else { return nil }
        try? manager.removeItem(at: file)
        let lines = text.split(whereSeparator: \.isNewline)
            .map { $0.trimmingCharacters(in: .whitespaces) }
        guard let uri = lines.first, uri.hasPrefix("waystone://pair") else { return nil }
        return PairingInbox(uri: uri, autoConnect: lines.dropFirst().contains("connect"))
    }
}
#endif
