import XCTest
@testable import Waystone

final class GestureEngineTests: XCTestCase {
    // Reference geometry: normalized coordinates are independent of the screen.
    private let start: (x: Double, y: Double) = (0.5, 0.5)

    func testShortStationaryPressIsAClick() {
        var engine = GestureEngine()
        XCTAssertTrue(engine.touchDown(x: start.x, y: start.y, at: 0).isEmpty)
        let outputs = engine.touchUp(x: start.x, y: start.y, at: 0.1)
        XCTAssertEqual(outputs, [
            .move(x: start.x, y: start.y),
            .leftDown(x: start.x, y: start.y),
            .leftUp(x: start.x, y: start.y),
        ])
    }

    func testMovingEarlyHoversAndNeverClicks() {
        var engine = GestureEngine()
        _ = engine.touchDown(x: 0.2, y: 0.5, at: 0)
        // Cross the 8 pt tolerance well within the long press window.
        let moveOutputs = engine.touchMove(x: 0.4, y: 0.5, at: 0.02)
        XCTAssertEqual(moveOutputs, [.move(x: 0.4, y: 0.5)])

        let more = engine.touchMove(x: 0.6, y: 0.5, at: 0.05)
        XCTAssertEqual(more, [.move(x: 0.6, y: 0.5)])

        // Lifting after a hover must not emit any button event: this is the rule
        // that stops accidental card plays.
        let up = engine.touchUp(x: 0.6, y: 0.5, at: 0.08)
        XCTAssertTrue(up.isEmpty)
    }

    func testLongPressStartsADragWithAHaptic() {
        var engine = GestureEngine()
        _ = engine.touchDown(x: 0.5, y: 0.5, at: 0)
        // A tiny move inside the tolerance keeps the candidate a press.
        _ = engine.touchMove(x: 0.501, y: 0.5, at: 0.05)
        let outputs = engine.touchMove(x: 0.501, y: 0.501, at: 0.17)
        XCTAssertEqual(outputs, [
            .move(x: 0.501, y: 0.501),
            .leftDown(x: 0.501, y: 0.501),
            .haptic(.grab),
        ])

        let dragged = engine.touchMove(x: 0.6, y: 0.6, at: 0.3)
        XCTAssertEqual(dragged, [.move(x: 0.6, y: 0.6)])

        let released = engine.touchUp(x: 0.7, y: 0.7, at: 0.4)
        XCTAssertEqual(released, [.leftUp(x: 0.7, y: 0.7)])
    }

    func testTwoFingerTapIsARightClick() {
        var engine = GestureEngine()
        _ = engine.touchDown(x: 0.4, y: 0.5, at: 0)
        _ = engine.touchDown(x: 0.6, y: 0.5, at: 0.05)
        let outputs = engine.touchUp(x: 0.6, y: 0.5, at: 0.12)
        XCTAssertEqual(outputs, [.rightClick(x: 0.6, y: 0.5), .haptic(.confirm)])
    }

    func testSlowTwoFingerDoesNotRightClick() {
        var engine = GestureEngine()
        _ = engine.touchDown(x: 0.4, y: 0.5, at: 0)
        _ = engine.touchDown(x: 0.6, y: 0.5, at: 0.05)
        XCTAssertTrue(engine.touchUp(x: 0.6, y: 0.5, at: 0.5).isEmpty)
    }

    func testTwoFingerVerticalMovementIsWheel() {
        var engine = GestureEngine()
        _ = engine.touchDown(x: 0.4, y: 0.5, at: 0)
        _ = engine.touchDown(x: 0.6, y: 0.5, at: 0.02)
        // The first move only records the origin.
        XCTAssertTrue(engine.touchMove(x: 0.5, y: 0.5, at: 0.05).isEmpty)
        guard case .wheel(let dx, let dy) = engine.touchMove(x: 0.5, y: 0.6, at: 0.1).first else {
            return XCTFail("expected a wheel event")
        }
        XCTAssertEqual(dx, 0)
        XCTAssertGreaterThan(dy, 0)
    }

    func testSecondFingerDuringDragReleasesTheButton() {
        var engine = GestureEngine()
        _ = engine.touchDown(x: 0.5, y: 0.5, at: 0)
        _ = engine.touchMove(x: 0.5, y: 0.5, at: 0.2)
        let outputs = engine.touchDown(x: 0.6, y: 0.6, at: 0.3)
        // Dropping the second finger on the screen must not leave the host with a
        // stuck mouse button.
        XCTAssertEqual(outputs, [.leftUp(x: 0.5, y: 0.5)])
    }

    func testPinchIsLocalAndClamped() {
        var engine = GestureEngine()
        XCTAssertEqual(engine.pinch(scale: 2, ended: false), 2)
        XCTAssertEqual(engine.pinch(scale: 10, ended: false), 4)
        _ = engine.pinch(scale: 4, ended: true)
        // A new pinch starts from the committed zoom.
        XCTAssertEqual(engine.pinch(scale: 0.5, ended: false), 2)
        engine.resetZoom()
        XCTAssertEqual(engine.zoom, 1)
    }

    func testCancelReleasesADrag() {
        var engine = GestureEngine()
        _ = engine.touchDown(x: 0.5, y: 0.5, at: 0)
        _ = engine.touchMove(x: 0.5, y: 0.5, at: 0.2)
        XCTAssertEqual(engine.cancel(), [.leftUp(x: 0.5, y: 0.5)])
        XCTAssertEqual(engine.state, .idle)
    }
}
