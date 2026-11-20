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
                .task {

                    #if DEBUG
                    // Development affordance: a simulator cannot scan a QR code,
                    // so the pairing URI can be injected with
                    // `simctl launch --setenv SIMCTL_CHILD_WAYSTONE_PAIR_URI=...`.
                    // The variable is ignored in release builds.
                    if let uri = ProcessInfo.processInfo.environment["WAYSTONE_PAIR_URI"],
                        store.instances.isEmpty {
                        controller.pair(with: uri)
                    }
                    // After pairing, optionally auto-connect to the first online
                    // host so a headless run can exercise the media path.
                    if ProcessInfo.processInfo.environment["WAYSTONE_AUTO_CONNECT"] != nil {
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
