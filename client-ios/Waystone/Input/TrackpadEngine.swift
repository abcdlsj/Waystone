import CoreGraphics
import Foundation

/// Trackpad mode: the screen is a laptop touchpad. Design doc section 5.2.
///
/// One finger moves the cursor relatively (with acceleration), a quick tap
/// left clicks where the cursor is, and holding still before moving grabs for
/// a drag. Two fingers tap for a right click, scroll, or pinch to zoom.
struct TrackpadEngine: TouchEngine {
    enum State: Equatable {
        case idle
        case pending(touch: Int, start: CGPoint, at: TimeInterval)
        case moving(touch: Int)
        case dragging(touch: Int)
        case twoFingers(TwoFingerGesture)
        case draining
    }

    /// Grabbing on a trackpad needs a firmer hold than direct touch, because
    /// resting a finger before moving is normal there.
    static let holdDuration: TimeInterval = 0.35
    /// Quick taps click; anything slower that did not move does nothing.
    static let tapDuration: TimeInterval = 0.25

    private(set) var state: State = .idle
    private var active: [Int: CGPoint] = [:]
    private var lastMove: (point: CGPoint, time: TimeInterval)?

    var deadline: TimeInterval? {
        if case .pending(_, _, let start) = state { return start + Self.holdDuration }
        return nil
    }

    mutating func began(_ touch: Int, at point: CGPoint, time: TimeInterval) -> [PointerOutput] {
        active[touch] = point
        switch state {
        case .idle:
            state = .pending(touch: touch, start: point, at: time)
            lastMove = (point, time)
            return []
        case .pending(let first, _, _), .moving(let first):
            guard let firstPoint = active[first] else { return [] }
            state = .twoFingers(TwoFingerGesture((first, firstPoint), (touch, point), time: time))
            return []
        case .dragging, .twoFingers, .draining:
            return []
        }
    }

    mutating func moved(_ touches: [Int: CGPoint], time: TimeInterval) -> [PointerOutput] {
        active.merge(touches) { $1 }
        switch state {
        case .pending(let touch, let start, _):
            guard let point = touches[touch], distance(point, start) > TouchTuning.slop else { return [] }
            state = .moving(touch: touch)
            return relative(to: point, time: time)
        case .moving(let touch), .dragging(let touch):
            guard let point = touches[touch] else { return [] }
            return relative(to: point, time: time)
        case .twoFingers(var gesture):
            let outputs = gesture.moved(active)
            state = .twoFingers(gesture)
            return outputs
        case .idle, .draining:
            return []
        }
    }

    mutating func ended(_ touch: Int, at point: CGPoint, time: TimeInterval) -> [PointerOutput] {
        active[touch] = nil
        defer { if active.isEmpty { state = .idle } }
        switch state {
        case .pending(let pressed, _, let startedAt) where pressed == touch:
            state = .idle
            return time - startedAt < Self.tapDuration ? [.click(.left, at: .cursor)] : []
        case .moving(let moving) where moving == touch:
            state = .idle
            return []
        case .dragging(let dragging) where dragging == touch:
            state = .draining
            return [.release(.left, at: .cursor)]
        case .twoFingers(let gesture):
            state = .draining
            return gesture.isTap(endedAt: time) ? [.click(.right, at: .cursor), .haptic(.secondary)] : []
        default:
            return []
        }
    }

    mutating func cancelled() -> [PointerOutput] {
        active.removeAll()
        defer { state = .idle }
        if case .dragging = state { return [.release(.left, at: .cursor)] }
        return []
    }

    mutating func tick(time: TimeInterval) -> [PointerOutput] {
        guard case .pending(let touch, _, let startedAt) = state,
            time - startedAt >= Self.holdDuration
        else { return [] }
        state = .dragging(touch: touch)
        return [.press(.left, at: .cursor), .haptic(.grab)]
    }

    /// Converts finger travel into accelerated cursor travel: slow motion is
    /// precise, a flick crosses the screen.
    private mutating func relative(to point: CGPoint, time: TimeInterval) -> [PointerOutput] {
        guard let last = lastMove else {
            lastMove = (point, time)
            return []
        }
        lastMove = (point, time)
        let delta = CGVector(dx: point.x - last.point.x, dy: point.y - last.point.y)
        let elapsed = max(time - last.time, 1.0 / 240)
        let speed = hypot(delta.dx, delta.dy) / elapsed
        let gain = Self.gain(forSpeed: speed)
        return [.moveBy(CGVector(dx: delta.dx * gain, dy: delta.dy * gain))]
    }

    /// Points per second to multiplier; linear between a precise floor and a
    /// fast ceiling.
    static func gain(forSpeed speed: CGFloat) -> CGFloat {
        min(max(0.8 + speed / 700, 0.8), 3)
    }
}
