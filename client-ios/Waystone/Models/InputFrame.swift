import Foundation

/// Input frames matching `crates/waystone-proto/src/input.rs`.
///
/// The encoder is hand written for the same reason as the Rust side: the layout
/// must stay auditable and identical on both ends.
enum InputFrame {
    static let axisMax: Double = 65535

    enum Button: UInt8 {
        case left = 0
        case right = 1
        case middle = 2
    }

    static func mouseMove(x: Double, y: Double) -> Data {
        var data = Data([0x01])
        data.append(contentsOf: withUnsafeBytes(of: UInt16(clamping: Int((x * axisMax).rounded())).bigEndian) { Array($0) })
        data.append(contentsOf: withUnsafeBytes(of: UInt16(clamping: Int((y * axisMax).rounded())).bigEndian) { Array($0) })
        return data
    }

    static func mouseButton(_ button: Button, down: Bool, x: Double, y: Double) -> Data {
        var data = Data([0x02, button.rawValue, down ? 1 : 0])
        data.append(contentsOf: withUnsafeBytes(of: UInt16(clamping: Int((x * axisMax).rounded())).bigEndian) { Array($0) })
        data.append(contentsOf: withUnsafeBytes(of: UInt16(clamping: Int((y * axisMax).rounded())).bigEndian) { Array($0) })
        return data
    }

    static func wheel(dx: Int16, dy: Int16) -> Data {
        var data = Data([0x03])
        data.append(contentsOf: withUnsafeBytes(of: UInt16(bitPattern: dx).bigEndian) { Array($0) })
        data.append(contentsOf: withUnsafeBytes(of: UInt16(bitPattern: dy).bigEndian) { Array($0) })
        return data
    }

    /// `code` is the macOS virtual key code; modifiers use the proto bit mask.
    static func key(code: UInt32, down: Bool, modifiers: UInt8) -> Data {
        var data = Data([0x04])
        data.append(contentsOf: withUnsafeBytes(of: code.bigEndian) { Array($0) })
        data.append(down ? 1 : 0)
        data.append(modifiers)
        return data
    }

    static func text(_ value: String) -> Data {
        var data = Data([0x05])
        data.append(contentsOf: Array(value.utf8))
        return data
    }

    /// Releases every key and button the host thinks is held.
    static func releaseAll() -> Data {
        Data([0x06])
    }

    /// Virtual gamepad sample; used by the on-screen pad preset.
    static func pad(lx: Int16, ly: Int16, rx: Int16, ry: Int16, buttons: UInt32) -> Data {
        var data = Data([0x07])
        for value in [UInt16(bitPattern: lx), UInt16(bitPattern: ly), UInt16(bitPattern: rx), UInt16(bitPattern: ry)] {
            data.append(contentsOf: withUnsafeBytes(of: value.bigEndian) { Array($0) })
        }
        data.append(contentsOf: withUnsafeBytes(of: buttons.bigEndian) { Array($0) })
        return data
    }

    /// Modifier bit mask shared with the host.
    enum Modifier: UInt8 {
        case shift = 1
        case control = 2
        case option = 4
        case command = 8
    }
}

/// Modifier anchors kept in sync with `InputFrame.Modifier`.
enum Modifiers {
    static let shift: UInt8 = 0x01
    static let control: UInt8 = 0x02
    static let option: UInt8 = 0x04
    static let command: UInt8 = 0x08
}
