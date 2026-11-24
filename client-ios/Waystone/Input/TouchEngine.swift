import CoreGraphics
import Foundation

/// What a touch engine wants done. Positions are surface points; the input
/// controller maps them onto the remote picture.
enum PointerOutput: Equatable {
    enum Target: Equatable {
        /// A point on the touch surface.
        case point(CGPoint)
        /// Wherever the remote cursor currently is (trackpad mode).
        case cursor
    }

    enum Haptic: Equatable {
        /// Something was grabbed: a drag started.
        case grab
        /// A secondary action fired: right click.
        case secondary
    }

    case move(to: CGPoint)
    /// Relative cursor motion in surface points, already accelerated.
    case moveBy(CGVector)
    case press(InputEvent.MouseButton, at: Target)
    case release(InputEvent.MouseButton, at: Target)
    case click(InputEvent.MouseButton, at: Target)
    /// Finger travel in surface points; positive `dy` means the fingers moved down.
    case scroll(dx: CGFloat, dy: CGFloat)
    /// Local zoom: multiply the scale by `factor` around `focus`, then pan.
    case zoom(factor: CGFloat, focus: CGPoint, translation: CGVector)
    case haptic(Haptic)
}

/// A touch-to-pointer state machine. Times are seconds on any monotonic clock.
protocol TouchEngine {
    mutating func began(_ touch: Int, at point: CGPoint, time: TimeInterval) -> [PointerOutput]
    mutating func moved(_ touches: [Int: CGPoint], time: TimeInterval) -> [PointerOutput]
    mutating func ended(_ touch: Int, at point: CGPoint, time: TimeInterval) -> [PointerOutput]
    /// The system took the touches away; held buttons must be released.
    mutating func cancelled() -> [PointerOutput]
    /// Lets time-based transitions (long press) fire without a move.
    mutating func tick(time: TimeInterval) -> [PointerOutput]
    /// When `tick` next needs to run, if ever.
    var deadline: TimeInterval? { get }
}

/// Thresholds shared by both engines; from the design doc, section 5.2.
enum TouchTuning {
    /// A press shorter than this is a click; holding it this long grabs.
    static let holdDuration: TimeInterval = 0.16
    /// A finger that drifts less than this is still "stationary", in points.
    static let slop: CGFloat = 8
    /// Two fingers down and up within this is a right click.
    static let twoFingerTapDuration: TimeInterval = 0.2
    /// Two-finger motion below this is still a tap, in points.
    static let twoFingerSlop: CGFloat = 10
}

/// Two fingers on the glass: tap (right click), scroll, or pinch.
///
/// The gesture commits once the fingers have clearly moved: if the distance
/// between them changes more than their midpoint travels it is a pinch,
/// otherwise a scroll. Committing once keeps a scroll from wobbling the zoom.
struct TwoFingerGesture: Equatable {
    enum Mode: Equatable { case undecided, scroll, pinch }

    let ids: (Int, Int)
    let startedAt: TimeInterval
    private(set) var mode: Mode = .undecided
    private let startCentroid: CGPoint
    private let startSpan: CGFloat
    private var lastCentroid: CGPoint
    private var lastSpan: CGFloat

    init(_ a: (Int, CGPoint), _ b: (Int, CGPoint), time: TimeInterval) {
        ids = (a.0, b.0)
        startedAt = time
        startCentroid = Self.centroid(a.1, b.1)
        startSpan = Self.span(a.1, b.1)
        lastCentroid = startCentroid
        lastSpan = startSpan
    }

    static func == (lhs: TwoFingerGesture, rhs: TwoFingerGesture) -> Bool {
        lhs.ids == rhs.ids && lhs.startedAt == rhs.startedAt && lhs.mode == rhs.mode
    }

    var centroid: CGPoint { lastCentroid }

    mutating func moved(_ touches: [Int: CGPoint]) -> [PointerOutput] {
        guard let a = touches[ids.0], let b = touches[ids.1] else { return [] }
        let centroid = Self.centroid(a, b)
        let span = Self.span(a, b)
        defer {
            lastCentroid = centroid
            lastSpan = span
        }
        if mode == .undecided {
            let travel = hypot(centroid.x - startCentroid.x, centroid.y - startCentroid.y)
            let stretch = abs(span - startSpan)
            guard max(travel, stretch) > TouchTuning.twoFingerSlop else { return [] }
            mode = stretch > travel ? .pinch : .scroll
        }
        switch mode {
        case .undecided:
            return []
        case .scroll:
            return [.scroll(dx: centroid.x - lastCentroid.x, dy: centroid.y - lastCentroid.y)]
        case .pinch:
            guard lastSpan > 0 else { return [] }
            return [
                .zoom(
                    factor: span / lastSpan, focus: lastCentroid,
                    translation: CGVector(dx: centroid.x - lastCentroid.x, dy: centroid.y - lastCentroid.y))
            ]
        }
    }

    /// True when the gesture ended as a quick, still two-finger tap.
    func isTap(endedAt time: TimeInterval) -> Bool {
        mode == .undecided && time - startedAt < TouchTuning.twoFingerTapDuration
    }

    private static func centroid(_ a: CGPoint, _ b: CGPoint) -> CGPoint {
        CGPoint(x: (a.x + b.x) / 2, y: (a.y + b.y) / 2)
    }

    private static func span(_ a: CGPoint, _ b: CGPoint) -> CGFloat {
        hypot(a.x - b.x, a.y - b.y)
    }
}

func distance(_ a: CGPoint, _ b: CGPoint) -> CGFloat {
    hypot(a.x - b.x, a.y - b.y)
}
