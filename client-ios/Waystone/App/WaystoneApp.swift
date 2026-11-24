import SwiftUI

@main
struct WaystoneApp: App {
    @UIApplicationDelegateAdaptor(AppDelegate.self) private var appDelegate
    @StateObject private var model = AppModel()
    @Environment(\.scenePhase) private var scenePhase

    var body: some Scene {
        WindowGroup {
            RootView()
                .environmentObject(model)
                .preferredColorScheme(.dark)
                .onOpenURL { url in
                    // Pairing links are `waystone://pair?...`, so the Camera app
                    // and Safari hand them straight to the app.
                    model.pair(with: url.absoluteString)
                }
                .task { DevHooks.run(model) }
        }
        .onChange(of: scenePhase) { _, phase in model.scenePhaseChanged(phase) }
    }
}

/// Home, with the stream covering it while one runs.
struct RootView: View {
    @EnvironmentObject private var model: AppModel

    var body: some View {
        HomeView()
            .fullScreenCover(item: Binding(get: { model.session }, set: { _ in })) { session in
                StreamView(session: session)
                    .environmentObject(model.preferences)
            }
    }
}

/// Orientation control: the home screen rotates freely, a stream is landscape.
final class AppDelegate: NSObject, UIApplicationDelegate {
    static var orientations: UIInterfaceOrientationMask = .all

    func application(
        _ application: UIApplication, supportedInterfaceOrientationsFor window: UIWindow?
    ) -> UIInterfaceOrientationMask {
        Self.orientations
    }

    @MainActor
    static func lock(_ mask: UIInterfaceOrientationMask) {
        orientations = mask
        for case let scene as UIWindowScene in UIApplication.shared.connectedScenes {
            scene.requestGeometryUpdate(.iOS(interfaceOrientations: mask))
            scene.keyWindow?.rootViewController?.setNeedsUpdateOfSupportedInterfaceOrientations()
        }
    }
}
