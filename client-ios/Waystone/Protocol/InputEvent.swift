import Foundation

/// Input frames, byte for byte the layout of `crates/waystone-proto/src/input.rs`.
///
/// Positions are normalized to the streamed picture (`0...1` here, `0...65535`
/// on the wire), so the host never needs to know the phone's geometry.
enum InputEvent: Equatable {
    enum MouseButton: UInt8 {
        case left = 0
        case right = 1
        case middle = 2
    }

    case mouseMove(x: Double, y: Double)
    case mouseButton(MouseButton, down: Bool, x: Double, y: Double)
    case wheel(dx: Int16, dy: Int16)
    case key(KeyCode, down: Bool, modifiers: Modifiers = [])
    case text(String)
    case releaseAll

    /// Edges must arrive; superseded samples may be dropped.
    var isReliable: Bool {
        if case .mouseMove = self { return false }
        return true
    }

    func encoded() -> Data {
        var out = Data()
        switch self {
        case .mouseMove(let x, let y):
            out.append(0x01)
            out.appendBigEndian(Self.axis(x))
            out.appendBigEndian(Self.axis(y))
        case .mouseButton(let button, let down, let x, let y):
            out.append(contentsOf: [0x02, button.rawValue, down ? 1 : 0])
            out.appendBigEndian(Self.axis(x))
            out.appendBigEndian(Self.axis(y))
        case .wheel(let dx, let dy):
            out.append(0x03)
            out.appendBigEndian(UInt16(bitPattern: dx))
            out.appendBigEndian(UInt16(bitPattern: dy))
        case .key(let code, let down, let modifiers):
            out.append(0x04)
            out.appendBigEndian(code.rawValue)
            out.append(contentsOf: [down ? 1 : 0, modifiers.rawValue])
        case .text(let value):
            out.append(0x05)
            out.append(contentsOf: Array(value.utf8))
        case .releaseAll:
            out.append(0x06)
        }
        return out
    }

    private static func axis(_ value: Double) -> UInt16 {
        UInt16((min(max(value, 0), 1) * 65535).rounded())
    }
}

/// Modifier bits, `waystone_proto::input::modifiers`.
struct Modifiers: OptionSet, Hashable {
    let rawValue: UInt8
    static let shift = Modifiers(rawValue: 1 << 0)
    static let control = Modifiers(rawValue: 1 << 1)
    static let alt = Modifiers(rawValue: 1 << 2)
    static let meta = Modifiers(rawValue: 1 << 3)
}

/// USB HID usage ids (Keyboard page), `waystone_proto::input::keys`.
///
/// The controller never sees the host's native key codes: the host maps these
/// onto macOS virtual keys or Windows scan codes.
struct KeyCode: RawRepresentable, Hashable {
    let rawValue: UInt32
    init(rawValue: UInt32) { self.rawValue = rawValue }

    static let enter = KeyCode(rawValue: 0x28)
    static let escape = KeyCode(rawValue: 0x29)
    static let backspace = KeyCode(rawValue: 0x2a)
    static let tab = KeyCode(rawValue: 0x2b)
    static let space = KeyCode(rawValue: 0x2c)
    static let right = KeyCode(rawValue: 0x4f)
    static let left = KeyCode(rawValue: 0x50)
    static let down = KeyCode(rawValue: 0x51)
    static let up = KeyCode(rawValue: 0x52)
    static let leftControl = KeyCode(rawValue: 0xe0)
    static let leftShift = KeyCode(rawValue: 0xe1)
    static let leftAlt = KeyCode(rawValue: 0xe2)
    static let leftMeta = KeyCode(rawValue: 0xe3)

    /// `a`...`z`, case insensitive.
    static func letter(_ character: Character) -> KeyCode? {
        guard let ascii = character.lowercased().first?.asciiValue,
            (97...122).contains(ascii)
        else { return nil }
        return KeyCode(rawValue: 0x04 + UInt32(ascii - 97))
    }

    /// `0`...`9`.
    static func digit(_ value: Int) -> KeyCode? {
        guard (0...9).contains(value) else { return nil }
        return KeyCode(rawValue: value == 0 ? 0x27 : 0x1d + UInt32(value))
    }

    static func function(_ number: Int) -> KeyCode? {
        guard (1...12).contains(number) else { return nil }
        return KeyCode(rawValue: 0x39 + UInt32(number))
    }

    /// The modifier this key is, if any; used to light up sticky modifiers.
    var modifier: Modifiers? {
        switch self {
        case .leftControl: return .control
        case .leftShift: return .shift
        case .leftAlt: return .alt
        case .leftMeta: return .meta
        default: return nil
        }
    }
}

extension Data {
    fileprivate mutating func appendBigEndian<T: FixedWidthInteger>(_ value: T) {
        Swift.withUnsafeBytes(of: value.bigEndian) { append(contentsOf: $0) }
    }
}
