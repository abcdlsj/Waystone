import UIKit

/// Taptic feedback for pointer gestures and tactile UI controls.
enum Haptics {
    private static let light = UIImpactFeedbackGenerator(style: .light)
    private static let medium = UIImpactFeedbackGenerator(style: .medium)
    private static let selector = UISelectionFeedbackGenerator()

    @MainActor
    static func play(_ kind: PointerOutput.Haptic) {
        switch kind {
        case .grab: light.impactOccurred()
        case .secondary: medium.impactOccurred()
        }
    }

    @MainActor
    static func selection() {
        selector.selectionChanged()
    }

    @MainActor
    static func tap() {
        light.impactOccurred(intensity: 0.65)
    }
}
