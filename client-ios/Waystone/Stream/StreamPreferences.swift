import Foundation

/// Stream options the user picks in the floating panel, remembered across
/// sessions.
@MainActor
final class StreamPreferences: ObservableObject {
    enum FrameRate: UInt32, CaseIterable, Identifiable {
        case fps30 = 30
        case fps45 = 45
        case fps60 = 60

        static let smooth = FrameRate.fps60
        static let cool = FrameRate.fps45

        var id: UInt32 { rawValue }
        var title: String { "\(rawValue) Hz" }
    }

    enum Resolution: String, CaseIterable, Identifiable {
        case auto
        case p720
        case p1080
        case p1440

        var id: String { rawValue }
        var title: String {
            switch self {
            case .auto: return "原生"
            case .p720: return "720p"
            case .p1080: return "1080p"
            case .p1440: return "2K"
            }
        }

        /// Landscape bounding box in pixels, or `nil` to follow the device screen.
        var maxSize: (width: UInt32, height: UInt32)? {
            switch self {
            case .auto: return nil
            case .p720: return (1280, 720)
            case .p1080: return (1920, 1080)
            case .p1440: return (2560, 1440)
            }
        }
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
    @Published var resolution: Resolution { didSet { save() } }
    @Published var sound: Sound { didSet { save() } }
    @Published var touchMode: InputController.Mode { didSet { save() } }
    @Published var presetId: String { didSet { save() } }
    /// Keeps the device screen awake while Waystone is open in the foreground.
    @Published var preventSleep: Bool { didSet { save() } }

    var preset: HotkeyPreset { HotkeyPreset.named(presetId) }

    private let defaults: UserDefaults

    init(defaults: UserDefaults = .standard) {
        self.defaults = defaults
        frameRate = FrameRate(rawValue: UInt32(defaults.integer(forKey: Keys.frameRate))) ?? .smooth
        resolution = defaults.string(forKey: Keys.resolution).flatMap(Resolution.init) ?? .auto
        sound = defaults.string(forKey: Keys.sound).flatMap(Sound.init) ?? .auto
        touchMode = defaults.string(forKey: Keys.touchMode).flatMap(InputController.Mode.init) ?? .direct
        presetId = defaults.string(forKey: Keys.preset) ?? HotkeyPreset.cards.id
        preventSleep = defaults.object(forKey: Keys.preventSleep) as? Bool ?? true
    }

    private func save() {
        defaults.set(Int(frameRate.rawValue), forKey: Keys.frameRate)
        defaults.set(resolution.rawValue, forKey: Keys.resolution)
        defaults.set(sound.rawValue, forKey: Keys.sound)
        defaults.set(touchMode.rawValue, forKey: Keys.touchMode)
        defaults.set(presetId, forKey: Keys.preset)
        defaults.set(preventSleep, forKey: Keys.preventSleep)
    }

    private enum Keys {
        static let frameRate = "stream.frameRate"
        static let resolution = "stream.resolution"
        static let sound = "stream.sound"
        static let touchMode = "stream.touchMode"
        static let preset = "stream.preset"
        static let preventSleep = "app.preventSleep"
    }
}
