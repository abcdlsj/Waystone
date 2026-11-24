import XCTest
@testable import Waystone

/// Every branch of the direct-touch table in design doc 5.2.
final class DirectTouchEngineTests: XCTestCase {
    private let a = CGPoint(x: 100, y: 100)

    func testQuickStillPressClicks() {
        var engine = DirectTouchEngine()
        XCTAssertEqual(engine.began(1, at: a, time: 0), [])
        XCTAssertEqual(engine.ended(1, at: a, time: 0.1), [.move(to: a), .click(.left, at: .point(a))])
    }

    func testMovingEarlyHoversAndLiftingNeverClicks() {
        var engine = DirectTouchEngine()
        _ = engine.began(1, at: a, time: 0)
        let b = CGPoint(x: 120, y: 100)
        XCTAssertEqual(engine.moved([1: b], time: 0.05), [.move(to: b)])
        let c = CGPoint(x: 200, y: 100)
        XCTAssertEqual(engine.moved([1: c], time: 0.3), [.move(to: c)])
        XCTAssertEqual(engine.ended(1, at: c, time: 0.4), [])
    }

    func testHoldingStillGrabsWithoutMovingThenDrops() {
        var engine = DirectTouchEngine()
        _ = engine.began(1, at: a, time: 0)
        XCTAssertEqual(engine.deadline, TouchTuning.holdDuration)
        // Nothing moved: the timer fires the grab.
        XCTAssertEqual(engine.tick(time: 0.1), [])
        XCTAssertEqual(engine.tick(time: 0.17), [.move(to: a), .press(.left, at: .point(a)), .haptic(.grab)])
        let b = CGPoint(x: 300, y: 200)
        XCTAssertEqual(engine.moved([1: b], time: 0.3), [.move(to: b)])
        XCTAssertEqual(engine.ended(1, at: b, time: 0.5), [.release(.left, at: .point(b))])
        XCTAssertEqual(engine.state, .idle)
    }

    func testDriftInsideSlopStillGrabs() {
        var engine = DirectTouchEngine()
        _ = engine.began(1, at: a, time: 0)
        let near = CGPoint(x: 104, y: 103)
        XCTAssertEqual(engine.moved([1: near], time: 0.05), [])
        XCTAssertEqual(engine.moved([1: near], time: 0.2), [.move(to: near), .press(.left, at: .point(near)), .haptic(.grab)])
    }

    func testTwoFingerTapRightClicksAtTheMidpoint() {
        var engine = DirectTouchEngine()
        _ = engine.began(1, at: CGPoint(x: 100, y: 100), time: 0)
        _ = engine.began(2, at: CGPoint(x: 200, y: 100), time: 0.03)
        let mid = CGPoint(x: 150, y: 100)
        XCTAssertEqual(
            engine.ended(1, at: CGPoint(x: 100, y: 100), time: 0.12),
            [.move(to: mid), .click(.right, at: .point(mid)), .haptic(.secondary)])
        // The second finger lifting does nothing more.
        XCTAssertEqual(engine.ended(2, at: CGPoint(x: 200, y: 100), time: 0.15), [])
        XCTAssertEqual(engine.state, .idle)
    }

    func testSlowTwoFingerTapDoesNothing() {
        var engine = DirectTouchEngine()
        _ = engine.began(1, at: CGPoint(x: 100, y: 100), time: 0)
        _ = engine.began(2, at: CGPoint(x: 200, y: 100), time: 0.01)
        XCTAssertEqual(engine.ended(1, at: CGPoint(x: 100, y: 100), time: 0.5), [])
    }

    func testParallelTwoFingerMotionScrolls() {
        var engine = DirectTouchEngine()
        _ = engine.began(1, at: CGPoint(x: 100, y: 100), time: 0)
        _ = engine.began(2, at: CGPoint(x: 200, y: 100), time: 0)
        // Commits once past the slop, then reports deltas.
        XCTAssertEqual(engine.moved([1: CGPoint(x: 100, y: 105), 2: CGPoint(x: 200, y: 105)], time: 0.05), [])
        XCTAssertEqual(
            engine.moved([1: CGPoint(x: 100, y: 120), 2: CGPoint(x: 200, y: 120)], time: 0.1),
            [.scroll(dx: 0, dy: 15)])
        XCTAssertEqual(engine.ended(1, at: .zero, time: 0.3), [])
    }

    func testSpreadingFingersZooms() {
        var engine = DirectTouchEngine()
        _ = engine.began(1, at: CGPoint(x: 100, y: 100), time: 0)
        _ = engine.began(2, at: CGPoint(x: 200, y: 100), time: 0)
        let outputs = engine.moved([1: CGPoint(x: 80, y: 100), 2: CGPoint(x: 220, y: 100)], time: 0.1)
        guard case .zoom(let factor, _, _)? = outputs.first else {
            return XCTFail("expected a zoom, got \(outputs)")
        }
        XCTAssertEqual(factor, 1.4, accuracy: 0.001)
    }

    func testCancellingADragReleasesTheButton() {
        var engine = DirectTouchEngine()
        _ = engine.began(1, at: a, time: 0)
        _ = engine.tick(time: 0.2)
        XCTAssertEqual(engine.cancelled(), [.release(.left, at: .point(a))])
        XCTAssertEqual(engine.state, .idle)
    }

    func testSecondFingerDoesNotDropAHeldCard() {
        var engine = DirectTouchEngine()
        _ = engine.began(1, at: a, time: 0)
        _ = engine.tick(time: 0.2)
        XCTAssertEqual(engine.began(2, at: CGPoint(x: 400, y: 400), time: 0.3), [])
        XCTAssertEqual(engine.ended(2, at: CGPoint(x: 400, y: 400), time: 0.35), [])
        XCTAssertEqual(engine.ended(1, at: a, time: 0.5), [.release(.left, at: .point(a))])
    }
}
