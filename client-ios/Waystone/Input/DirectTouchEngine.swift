import CoreGraphics
import Foundation

/// Direct touch: the finger is the cursor. Design doc section 5.2.
///
/// Card games break under a naive mapping, so a press has three outcomes:
///
/// * lift quickly without moving: click;
/// * move before the hold time: hover only, and lifting never clicks (reading
///   cards in hand must not play them);
/// * hold still for the hold time: grab (left button down) and drag.
///
/// Two fingers tap for a right click, scroll in parallel, or pinch to zoom the
/// local view.
struct DirectTouchEngine: TouchEngine {
    enum State: Equatable {
        case idle
        case pending(touch: Int, start: CGPoint, at: TimeInterval)
        case hovering(touch: Int)
        case dragging(touch: Int, last: CGPoint)
        case twoFingers(TwoFingerGesture)
        /// A multi-finger gesture ended; wait for every finger to lift.
        case draining
    }

    private(set) var state: State = .idle
    private var active: [Int: CGPoint] = [:]

    var deadline: TimeInterval? {
        if case .pending(_, _, let start) = state { return start + TouchTuning.holdDuration }
        return nil
    }

    mutating func began(_ touch: Int, at point: CGPoint, time: TimeInterval) -> [PointerOutput] {
        active[touch] = point
        switch state {
        case .idle:
            state = .pending(touch: touch, start: point, at: time)
            return []
        case .pending(let first, _, _), .hovering(let first):
            guard let firstPoint = active[first] else { return [] }
            state = .twoFingers(TwoFingerGesture((first, firstPoint), (touch, point), time: time))
            return []
        case .dragging, .twoFingers, .draining:
            // An extra finger never drops a held card.
            return []
        }
    }

    mutating func moved(_ touches: [Int: CGPoint], time: TimeInterval) -> [PointerOutput] {
        active.merge(touches) { $1 }
        switch state {
        case .pending(let touch, let start, let startedAt):
            guard let point = touches[touch] else { return [] }
            if distance(point, start) > TouchTuning.slop {
                state = .hovering(touch: touch)
                return [.move(to: point)]
            }
            if time - startedAt >= TouchTuning.holdDuration {
                return grab(touch: touch, at: point)
            }
            return []
        case .hovering(let touch):
            guard let point = touches[touch] else { return [] }
            return [.move(to: point)]
        case .dragging(let touch, _):
            guard let point = touches[touch] else { return [] }
            state = .dragging(touch: touch, last: point)
            return [.move(to: point)]
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
            guard time - startedAt < TouchTuning.holdDuration else {
                // The hold fired between frames; finish it as a grab and drop.
                return grab(touch: touch, at: point) + [.release(.left, at: .point(point))]
            }
            return [.move(to: point), .click(.left, at: .point(point))]
        case .hovering(let hovering) where hovering == touch:
            state = .idle
            return []
        case .dragging(let dragging, _) where dragging == touch:
            state = .draining
            return [.release(.left, at: .point(point))]
        case .twoFingers(let gesture):
            state = .draining
            guard gesture.isTap(endedAt: time) else { return [] }
            let at = gesture.centroid
            return [.move(to: at), .click(.right, at: .point(at)), .haptic(.secondary)]
        default:
            return []
        }
    }

    mutating func cancelled() -> [PointerOutput] {
        active.removeAll()
        defer { state = .idle }
        if case .dragging(_, let last) = state {
            return [.release(.left, at: .point(last))]
        }
        return []
    }

    mutating func tick(time: TimeInterval) -> [PointerOutput] {
        guard case .pending(let touch, _, let startedAt) = state,
            time - startedAt >= TouchTuning.holdDuration,
            let point = active[touch]
        else { return [] }
        return grab(touch: touch, at: point)
    }

    private mutating func grab(touch: Int, at point: CGPoint) -> [PointerOutput] {
        state = .dragging(touch: touch, last: point)
        return [.move(to: point), .press(.left, at: .point(point)), .haptic(.grab)]
    }
}
