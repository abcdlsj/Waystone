import UIKit

/// Taptic feedback for the two moments the design doc asks for.
enum Haptics {
    private static let light = UIImpactFeedbackGenerator(style: .light)
    private static let medium = UIImpactFeedbackGenerator(style: .medium)

    @MainActor
    static func play(_ kind: PointerOutput.Haptic) {
        switch kind {
        case .grab: light.impactOccurred()
        case .secondary: medium.impactOccurred()
        }
    }
}
