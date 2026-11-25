import Combine
import CoreGraphics
import Foundation

/// Turns touches and on-screen controls into input frames for the host.
///
/// It owns the touch engine of the current mode, the local zoom, the remote
/// cursor position and every key or button it is holding, so it can always
/// release what it pressed: on a mode change, a preset change, or when the
/// session ends.
@MainActor
final class InputController: ObservableObject {
    enum Mode: String, CaseIterable, Identifiable {
        case direct
        case trackpad

        var id: String { rawValue }
        var title: String { self == .direct ? "直点" : "触控板" }
    }

    @Published private(set) var geometry = PictureGeometry()
    @Published private(set) var mode: Mode
    /// Remote cursor, `0...1`; drawn locally in trackpad mode.
    @Published private(set) var cursor: (x: Double, y: Double) = (0.5, 0.5)
    /// True while a finger is actively hovering or dragging in direct mode.
    @Published private(set) var directPointerActive = false
    /// Keys latched by toggle capsules, so they can be drawn as active.
    @Published private(set) var latchedKeys: Set<KeyCode> = []
    /// One-shot modifiers armed from the soft keyboard bar.
    @Published private(set) var armedModifiers: Modifiers = []

    private var engine: TouchEngine
    private var activeTouches: Set<Int> = []
    private var heldKeys: Set<KeyCode> = []
    /// Where input frames go; the session points it at the transport.
    var sink: (InputEvent) -> Void = { _ in }
    private let haptics: (PointerOutput.Haptic) -> Void

    /// Wheel pixels per point of finger travel.
    static let scrollGain: CGFloat = 2

    init(mode: Mode, haptics: ((PointerOutput.Haptic) -> Void)? = nil) {
        self.mode = mode
        self.haptics = haptics ?? { Haptics.play($0) }
        self.engine = Self.makeEngine(mode)
    }

    // MARK: - Geometry

    func layout(bounds: CGSize) {
        guard geometry.bounds != bounds else { return }
        geometry.bounds = bounds
        geometry.resetZoom()
    }

    func videoSizeChanged(_ size: CGSize) {
        guard geometry.videoSize != size else { return }
        geometry.videoSize = size
        geometry.resetZoom()
    }

    func resetZoom() {
        geometry.resetZoom()
    }

    // MARK: - Touches

    var deadline: TimeInterval? { engine.deadline }

    func touchBegan(_ id: Int, at point: CGPoint, time: TimeInterval) {
        activeTouches.insert(id)
        apply(engine.began(id, at: point, time: time))
    }

    func touchesMoved(_ touches: [Int: CGPoint], time: TimeInterval) {
        apply(engine.moved(touches, time: time))
    }

    func touchEnded(_ id: Int, at point: CGPoint, time: TimeInterval) {
        activeTouches.remove(id)
        apply(engine.ended(id, at: point, time: time))
        if activeTouches.isEmpty {
            directPointerActive = false
        }
    }

    func touchesCancelled() {
        activeTouches.removeAll()
        directPointerActive = false
        apply(engine.cancelled())
    }

    func tick(time: TimeInterval) {
        apply(engine.tick(time: time))
    }

    func setMode(_ mode: Mode) {
        guard mode != self.mode else { return }
        activeTouches.removeAll()
        directPointerActive = false
        apply(engine.cancelled())
        self.mode = mode
        engine = Self.makeEngine(mode)
    }

    // MARK: - Controls

    func perform(_ action: KeyAction, down: Bool) {
        switch action {
        case .tap(let key):
            guard down else { return }
            tap(key)
        case .toggle(let key):
            guard down else { return }
            if latchedKeys.contains(key) {
                latchedKeys.remove(key)
                keyUp(key)
            } else {
                latchedKeys.insert(key)
                keyDown(key)
            }
        case .hold(let key):
            down ? keyDown(key) : keyUp(key)
        case .click(let button):
            guard down else { return }
            click(button, at: cursor)
            haptics(.secondary)
        }
    }

    /// Holds exactly `keys` down, releasing the rest; drives the joystick.
    func hold(_ keys: Set<KeyCode>, among all: Set<KeyCode>) {
        for key in all.subtracting(keys) where heldKeys.contains(key) { keyUp(key) }
        for key in keys where !heldKeys.contains(key) { keyDown(key) }
    }

    /// Arms or disarms a modifier for the next key typed on the soft keyboard.
    func toggleArmed(_ modifier: Modifiers) {
        if armedModifiers.contains(modifier) {
            armedModifiers.remove(modifier)
        } else {
            armedModifiers.insert(modifier)
        }
    }

    /// Sends a key with the armed modifiers, then disarms them.
    func tap(_ key: KeyCode) {
        let modifiers = armedModifiers
        armedModifiers = []
        let modifierKeys = Self.keys(for: modifiers)
        modifierKeys.forEach { keyDown($0) }
        keyDown(key, modifiers: modifiers)
        keyUp(key, modifiers: modifiers)
        modifierKeys.reversed().forEach { keyUp($0) }
    }

    /// Text from the soft keyboard. With a modifier armed, letters become
    /// shortcuts (Ctrl+C) instead of text.
    func type(_ text: String) {
        guard !armedModifiers.isEmpty else {
            sink(.text(text))
            return
        }
        for character in text {
            if let key = KeyCode.letter(character) ?? character.wholeNumberValue.flatMap(KeyCode.digit) {
                let armed = armedModifiers
                tap(key)
                armedModifiers = armed
            } else {
                sink(.text(String(character)))
            }
        }
        armedModifiers = []
    }

    /// Lets go of everything this controller holds and tells the host to do
    /// the same, which also covers anything lost in flight.
    func releaseAll() {
        apply(engine.cancelled())
        heldKeys.removeAll()
        latchedKeys.removeAll()
        armedModifiers = []
        sink(.releaseAll)
    }

    // MARK: - Output

    private func apply(_ outputs: [PointerOutput]) {
        for output in outputs {
            switch output {
            case .move(let point):
                cursor = geometry.normalized(point)
                if mode == .direct && !activeTouches.isEmpty {
                    directPointerActive = true
                }
                sink(.mouseMove(x: cursor.x, y: cursor.y))
            case .moveBy(let delta):
                let size = geometry.displayedSize
                guard size.width > 0, size.height > 0 else { continue }
                cursor = (
                    min(max(cursor.x + Double(delta.dx / size.width), 0), 1),
                    min(max(cursor.y + Double(delta.dy / size.height), 0), 1)
                )
                sink(.mouseMove(x: cursor.x, y: cursor.y))
            case .press(let button, let target):
                let at = position(target)
                sink(.mouseButton(button, down: true, x: at.x, y: at.y))
            case .release(let button, let target):
                let at = position(target)
                sink(.mouseButton(button, down: false, x: at.x, y: at.y))
            case .click(let button, let target):
                click(button, at: position(target))
            case .scroll(let dx, let dy):
                let wheelX = Int16(clamping: Int((dx * Self.scrollGain).rounded()))
                let wheelY = Int16(clamping: Int((dy * Self.scrollGain).rounded()))
                guard wheelX != 0 || wheelY != 0 else { continue }
                sink(.wheel(dx: wheelX, dy: wheelY))
            case .zoom(let factor, let focus, let translation):
                geometry.zoom(by: factor, around: focus, translation: translation)
            case .haptic(let kind):
                haptics(kind)
            }
        }
    }

    private func position(_ target: PointerOutput.Target) -> (x: Double, y: Double) {
        switch target {
        case .point(let point):
            cursor = geometry.normalized(point)
        case .cursor:
            break
        }
        return cursor
    }

    private func click(_ button: InputEvent.MouseButton, at point: (x: Double, y: Double)) {
        // The host holds the button for a frame between the two edges, so
        // games that poll button state still see the click.
        sink(.mouseButton(button, down: true, x: point.x, y: point.y))
        sink(.mouseButton(button, down: false, x: point.x, y: point.y))
    }

    private func keyDown(_ key: KeyCode, modifiers: Modifiers = []) {
        heldKeys.insert(key)
        sink(.key(key, down: true, modifiers: modifiers))
    }

    private func keyUp(_ key: KeyCode, modifiers: Modifiers = []) {
        heldKeys.remove(key)
        sink(.key(key, down: false, modifiers: modifiers))
    }

    private static func keys(for modifiers: Modifiers) -> [KeyCode] {
        var keys: [KeyCode] = []
        if modifiers.contains(.control) { keys.append(.leftControl) }
        if modifiers.contains(.alt) { keys.append(.leftAlt) }
        if modifiers.contains(.shift) { keys.append(.leftShift) }
        if modifiers.contains(.meta) { keys.append(.leftMeta) }
        return keys
    }

    private static func makeEngine(_ mode: Mode) -> TouchEngine {
        switch mode {
        case .direct: return DirectTouchEngine()
        case .trackpad: return TrackpadEngine()
        }
    }
}
