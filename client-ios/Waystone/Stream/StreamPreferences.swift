import Foundation

/// Stream options the user picks in the floating panel, remembered across
/// sessions.
@MainActor
final class StreamPreferences: ObservableObject {
    enum FrameRate: UInt32, CaseIterable, Identifiable {
        case smooth = 60
        case cool = 45

        var id: UInt32 { rawValue }
        var title: String { self == .smooth ? "60 帧丝滑" : "45 帧低温" }
    }

    enum Sound: String, CaseIterable, Identifiable {
        /// Plays unless the ring/silent switch is on silent.
        case auto
        case on
        case off

        var id: String { rawValue }
        var title: String {
            switch self {
            case .auto: return "跟随静音键"
            case .on: return "开"
            case .off: return "关"
            }
        }
    }

    @Published var frameRate: FrameRate { didSet { save() } }
    @Published var sound: Sound { didSet { save() } }
    @Published var touchMode: InputController.Mode { didSet { save() } }
    @Published var presetId: String { didSet { save() } }

    var preset: HotkeyPreset { HotkeyPreset.named(presetId) }

    private let defaults: UserDefaults

    init(defaults: UserDefaults = .standard) {
        self.defaults = defaults
        frameRate = FrameRate(rawValue: UInt32(defaults.integer(forKey: Keys.frameRate))) ?? .smooth
        sound = defaults.string(forKey: Keys.sound).flatMap(Sound.init) ?? .auto
        touchMode = defaults.string(forKey: Keys.touchMode).flatMap(InputController.Mode.init) ?? .direct
        presetId = defaults.string(forKey: Keys.preset) ?? HotkeyPreset.cards.id
    }

    private func save() {
        defaults.set(Int(frameRate.rawValue), forKey: Keys.frameRate)
        defaults.set(sound.rawValue, forKey: Keys.sound)
        defaults.set(touchMode.rawValue, forKey: Keys.touchMode)
        defaults.set(presetId, forKey: Keys.preset)
    }

    private enum Keys {
        static let frameRate = "stream.frameRate"
        static let sound = "stream.sound"
        static let touchMode = "stream.touchMode"
        static let preset = "stream.preset"
    }
}
