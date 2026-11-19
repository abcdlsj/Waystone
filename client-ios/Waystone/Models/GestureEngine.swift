import Foundation

/// The direct-touch gesture state machine from the design doc.
///
/// The rules exist because card games break under naive touch mapping: a finger
/// that drags across the hand to read tooltips must never play a card, and a
/// deliberate long press must grab a card instead of tapping it.
///
/// All timings are wall clock based and every transition is unit tested, because
/// this is the part of the app users feel most directly.
struct GestureEngine {
    /// How long a press may last before it becomes a long press.
    static let longPressDuration: TimeInterval = 0.16
    /// How far a finger may drift and still count as a stationary press.
    static let moveTolerance: CGFloat = 8
    /// Two finger taps must be shorter than this.
    static let twoFingerTapDuration: TimeInterval = 0.2

    enum State: Equatable {
        /// Nothing is down.
        case idle
        /// A single finger is down but has not committed to a gesture yet.
        case pending(startX: Double, startY: Double, startedAt: TimeInterval)
        /// The finger moved; send hover moves only.
        case hovering(lastX: Double, lastY: Double)
        /// A long press committed to a drag; left button is held.
        case dragging(lastX: Double, lastY: Double)
        /// Two fingers are down, possible tap or pinch.
        case twoFingers(startedAt: TimeInterval)
    }

    /// What the caller should send to the host.
    enum Output: Equatable {
        case none
        case move(x: Double, y: Double)
        case leftDown(x: Double, y: Double)
        case leftUp(x: Double, y: Double)
        case rightClick(x: Double, y: Double)
        case wheel(dx: Int16, dy: Int16)
        case haptic(Haptic)
    }

    enum Haptic: Equatable {
        /// Fired when a drag grabs an item.
        case grab
        /// Fired when a two finger tap turns into a right click.
        case confirm
    }

    private(set) var state: State = .idle
    /// Local view scale, changed by pinch; never affects the host.
    private(set) var zoom: CGFloat = 1
    private var zoomAnchor: CGFloat = 1
    private var lastTwoFingerY: Double?

    /// Current pointer position, normalized.
    private(set) var pointer: (x: Double, y: Double) = (0.5, 0.5)

    /// A finger went down at a normalized position.
    mutating func touchDown(x: Double, y: Double, at time: TimeInterval) -> [Output] {
        switch state {
        case .idle:
            state = .pending(startX: x, startY: y, startedAt: time)
            pointer = (x, y)
            return []
        case .pending, .hovering, .dragging:
            // A second finger turns the gesture into a two finger one.
            if case .dragging = state {
                return releaseDrag()
            }
            state = .twoFingers(startedAt: time)
            lastTwoFingerY = y
            return []
        case .twoFingers:
            return []
        }
    }

    /// A finger moved; `count` is how many fingers are currently down.
    mutating func touchMove(x: Double, y: Double, at time: TimeInterval) -> [Output] {
        pointer = (x, y)
        switch state {
        case .pending(let startX, let startY, let startedAt):
            let distance = hypot((x - startX) * Self.referenceWidth, (y - startY) * Self.referenceHeight)
            if distance > Self.moveTolerance {
                // Moving early means the user wants to hover, not to click.
                state = .hovering(lastX: x, lastY: y)
                return [.move(x: x, y: y)]
            }
            if time - startedAt >= Self.longPressDuration {
                state = .dragging(lastX: x, lastY: y)
                return [.move(x: x, y: y), .leftDown(x: x, y: y), .haptic(.grab)]
            }
            return []
        case .hovering:
            state = .hovering(lastX: x, lastY: y)
            return [.move(x: x, y: y)]
        case .dragging:
            state = .dragging(lastX: x, lastY: y)
            return [.move(x: x, y: y)]
        case .twoFingers:
            guard let previous = lastTwoFingerY else {
                lastTwoFingerY = y
                return []
            }
            lastTwoFingerY = y
            // Vertical two finger movement becomes a wheel, which is what reads
            // logs and card lists naturally.
            let delta = (y - previous) * 240
            if abs(delta) < 1 {
                return []
            }
            return [.wheel(dx: 0, dy: Int16(clamping: Int(delta)))]
        case .idle:
            return []
        }
    }

    /// A finger lifted.
    mutating func touchUp(x: Double, y: Double, at time: TimeInterval) -> [Output] {
        switch state {
        case .pending(_, _, let startedAt):
            state = .idle
            // A short, stationary press is a click.
            guard time - startedAt < Self.longPressDuration else { return [] }
            return [
                .move(x: x, y: y),
                .leftDown(x: x, y: y),
                .leftUp(x: x, y: y),
            ]
        case .hovering:
            // Releasing after hovering must never click: this is the rule that
            // prevents accidental card plays.
            state = .idle
            return []
        case .dragging:
            // Release at the finger's final position, so a drag that ends with a
            // quick flick still lands where the user let go.
            state = .idle
            return [.leftUp(x: x, y: y)]
        case .twoFingers(let startedAt):
            state = .idle
            guard time - startedAt < Self.twoFingerTapDuration else { return [] }
            return [.rightClick(x: x, y: y), .haptic(.confirm)]
        case .idle:
            return []
        }
    }

    /// The whole gesture was cancelled, for example by a system gesture.
    mutating func cancel() -> [Output] {
        switch state {
        case .dragging:
            return releaseDrag()
        default:
            state = .idle
            return []
        }
    }

    /// Applies a pinch scale from a `UIPinchGestureRecognizer`.
    mutating func pinch(scale: CGFloat, ended: Bool) -> CGFloat {
        if !ended {
            zoom = min(max(zoomAnchor * scale, 1), 4)
        } else {
            zoomAnchor = zoom
        }
        return zoom
    }

    /// Resets the local zoom back to 1.
    mutating func resetZoom() {
        zoom = 1
        zoomAnchor = 1
    }

    private mutating func releaseDrag() -> [Output] {
        let last: (Double, Double)
        if case .dragging(let x, let y) = state {
            last = (x, y)
        } else {
            last = pointer
        }
        state = .idle
        return [.leftUp(x: last.0, y: last.1)]
    }

    /// Normalized distance thresholds make the gesture feel identical on a phone
    /// and an iPad, so `moveTolerance` is expressed in a 1000 pt reference space.
    private static let referenceWidth: CGFloat = 1000
    private static let referenceHeight: CGFloat = 1000
}
