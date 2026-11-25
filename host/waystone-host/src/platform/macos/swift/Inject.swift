// CoreGraphics input injection primitives.
//
// Decoding, key mapping, held-key tracking and click timing live in Rust
// (`src/input.rs`); this file only turns already-decided events into
// CGEvents, because only CoreGraphics can post them.

import ApplicationServices
import CoreGraphics
import Foundation

private let source = CGEventSource(stateID: .hidSystemState)

private func flags(_ modifiers: UInt8) -> CGEventFlags {
    var flags: CGEventFlags = []
    if modifiers & 0x01 != 0 { flags.insert(.maskShift) }
    if modifiers & 0x02 != 0 { flags.insert(.maskControl) }
    if modifiers & 0x04 != 0 { flags.insert(.maskAlternate) }
    if modifiers & 0x08 != 0 { flags.insert(.maskCommand) }
    return flags
}

private func mouseButton(_ button: Int32) -> CGMouseButton {
    switch button {
    case 1: return .right
    case 2: return .center
    case 3: return CGMouseButton(rawValue: 3)!
    case 4: return CGMouseButton(rawValue: 4)!
    default: return .left
    }
}

private func mouseTypes(_ button: Int32) -> (down: CGEventType, up: CGEventType, drag: CGEventType) {
    switch button {
    case 0: return (.leftMouseDown, .leftMouseUp, .leftMouseDragged)
    case 1: return (.rightMouseDown, .rightMouseUp, .rightMouseDragged)
    default: return (.otherMouseDown, .otherMouseUp, .otherMouseDragged)
    }
}

@_cdecl("ws_inject_mouse_move")
public func ws_inject_mouse_move(_ x: Double, _ y: Double, _ heldButton: Int32, _ modifiers: UInt8) {
    let point = CGPoint(x: x, y: y)
    let type: CGEventType = heldButton < 0 ? .mouseMoved : mouseTypes(heldButton).drag
    let button = heldButton < 0 ? CGMouseButton.left : mouseButton(heldButton)
    guard let event = CGEvent(mouseEventSource: source, mouseType: type, mouseCursorPosition: point, mouseButton: button)
    else { return }
    event.flags = flags(modifiers)
    event.post(tap: .cghidEventTap)
}

@_cdecl("ws_inject_mouse_button")
public func ws_inject_mouse_button(
    _ button: Int32, _ down: Bool, _ x: Double, _ y: Double, _ clicks: Int32, _ modifiers: UInt8
) {
    let types = mouseTypes(button)
    guard let event = CGEvent(
        mouseEventSource: source, mouseType: down ? types.down : types.up,
        mouseCursorPosition: CGPoint(x: x, y: y), mouseButton: mouseButton(button))
    else { return }
    // Without the click state a double click arrives as two single clicks.
    event.setIntegerValueField(.mouseEventClickState, value: Int64(max(clicks, 1)))
    if button > 2 { event.setIntegerValueField(.mouseEventButtonNumber, value: Int64(button)) }
    event.flags = flags(modifiers)
    event.post(tap: .cghidEventTap)
}

@_cdecl("ws_inject_scroll")
public func ws_inject_scroll(_ dx: Int32, _ dy: Int32) {
    guard let event = CGEvent(
        scrollWheelEvent2Source: source, units: .pixel, wheelCount: 2, wheel1: dy, wheel2: dx, wheel3: 0)
    else { return }
    event.post(tap: .cghidEventTap)
}

@_cdecl("ws_inject_key")
public func ws_inject_key(_ virtualKey: UInt16, _ down: Bool, _ modifiers: UInt8) {
    guard let event = CGEvent(keyboardEventSource: source, virtualKey: CGKeyCode(virtualKey), keyDown: down)
    else { return }
    event.flags = flags(modifiers)
    event.post(tap: .cghidEventTap)
}

/// Types Unicode text. Each character is a down/up pair carrying the string,
/// so no physical key stays logically pressed afterwards.
@_cdecl("ws_inject_text")
public func ws_inject_text(_ utf8: UnsafePointer<UInt8>?, _ length: Int) {
    guard let utf8, length > 0 else { return }
    let text = String(decoding: UnsafeBufferPointer(start: utf8, count: length), as: UTF8.self)
    for character in text {
        var units = Array(String(character).utf16)
        for down in [true, false] {
            guard let event = CGEvent(keyboardEventSource: source, virtualKey: 0, keyDown: down) else { continue }
            event.keyboardSetUnicodeString(stringLength: units.count, unicodeString: &units)
            event.post(tap: .cghidEventTap)
        }
    }
}

/// Posting events needs the Accessibility permission; without it CoreGraphics
/// silently drops them, so the agent checks at startup and says so.
@_cdecl("ws_accessibility_trusted")
public func ws_accessibility_trusted(_ prompt: Bool) -> Bool {
    let options = [kAXTrustedCheckOptionPrompt.takeUnretainedValue() as String: prompt] as CFDictionary
    return AXIsProcessTrustedWithOptions(options)
}
