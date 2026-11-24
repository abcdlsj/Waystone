import CoreGraphics

/// What an on-screen control sends.
enum KeyAction: Equatable {
    /// Press and release.
    case tap(KeyCode)
    /// First tap holds the key down, second tap releases it (Ctrl to skip).
    case toggle(KeyCode)
    /// Down while the finger is down.
    case hold(KeyCode)
    /// A mouse click at the current cursor.
    case click(InputEvent.MouseButton)
}

/// One capsule button.
struct HotkeyButton: Identifiable, Equatable {
    var id: String { title }
    var title: String
    /// Key cap shown next to the title, e.g. `Space`.
    var cap: String?
    var action: KeyAction
}

/// A cluster of capsules anchored to part of the screen.
struct HotkeyGroup: Identifiable, Equatable {
    enum Anchor: Equatable {
        case trailing, leading, top, bottomTrailing
    }

    var id: String { buttons.map(\.id).joined(separator: "|") }
    var anchor: Anchor
    var buttons: [HotkeyButton]
}

/// A near-invisible strip along a screen edge; thumbs hit it without looking.
struct EdgeZone: Identifiable, Equatable {
    enum Side: Equatable { case leading, trailing }
    var id: String { side == .leading ? "leading" : "trailing" }
    var side: Side
    var width: CGFloat
    var action: KeyAction
}

/// An on-screen stick that holds direction keys.
struct JoystickLayout: Equatable {
    var up: KeyCode
    var left: KeyCode
    var down: KeyCode
    var right: KeyCode
}

/// The built-in hotkey sets from design doc section 5.3.
///
/// Presets are plain data so new ones (or user-defined ones, later) need no
/// new views.
struct HotkeyPreset: Identifiable, Equatable {
    var id: String
    var name: String
    var symbol: String
    var groups: [HotkeyGroup] = []
    var edgeZones: [EdgeZone] = []
    var joystick: JoystickLayout?

    static let cards = HotkeyPreset(
        id: "cards", name: "卡牌", symbol: "suit.spade",
        groups: [
            HotkeyGroup(
                anchor: .trailing,
                buttons: [
                    HotkeyButton(title: "结束回合", cap: "Space", action: .tap(.space)),
                    HotkeyButton(title: "取消", cap: "右键", action: .click(.right)),
                    HotkeyButton(title: "菜单", cap: "Esc", action: .tap(.escape)),
                ])
        ])

    static let novel = HotkeyPreset(
        id: "novel", name: "视觉小说", symbol: "book",
        groups: [
            HotkeyGroup(
                anchor: .top,
                buttons: [HotkeyButton(title: "快进", cap: "Ctrl", action: .toggle(.leftControl))])
        ],
        edgeZones: [
            EdgeZone(side: .leading, width: 48, action: .tap(.enter)),
            EdgeZone(side: .trailing, width: 48, action: .tap(.enter)),
        ])

    static let routine = HotkeyPreset(
        id: "routine", name: "日常", symbol: "gamecontroller",
        groups: [
            HotkeyGroup(
                anchor: .bottomTrailing,
                buttons: [
                    HotkeyButton(title: "1", action: .tap(KeyCode.digit(1)!)),
                    HotkeyButton(title: "2", action: .tap(KeyCode.digit(2)!)),
                    HotkeyButton(title: "3", action: .tap(KeyCode.digit(3)!)),
                    HotkeyButton(title: "4", action: .tap(KeyCode.digit(4)!)),
                    HotkeyButton(title: "地图", cap: "M", action: .tap(KeyCode.letter("m")!)),
                    HotkeyButton(title: "跳跃", cap: "Space", action: .tap(.space)),
                    HotkeyButton(title: "交互", cap: "F", action: .tap(KeyCode.letter("f")!)),
                ])
        ],
        joystick: JoystickLayout(
            up: KeyCode.letter("w")!, left: KeyCode.letter("a")!,
            down: KeyCode.letter("s")!, right: KeyCode.letter("d")!))

    static let none = HotkeyPreset(id: "none", name: "无", symbol: "circle.slash")

    static let all: [HotkeyPreset] = [.cards, .novel, .routine, .none]

    static func named(_ id: String) -> HotkeyPreset {
        all.first { $0.id == id } ?? .cards
    }
}
