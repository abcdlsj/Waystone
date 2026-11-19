import SwiftUI

@main
struct WaystoneApp: App {
    @StateObject private var store = InstanceStore()
    @StateObject private var controller = SessionController()

    var body: some Scene {
        WindowGroup {
            HomeView()
                .environmentObject(store)
                .environmentObject(controller)
                .preferredColorScheme(.dark)
                .statusBarHidden(controller.phase.isStreaming)
        }
    }
}
