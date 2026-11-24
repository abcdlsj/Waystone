import XCTest
@testable import Waystone

final class TrackpadEngineTests: XCTestCase {
    func testTapClicksAtTheCursor() {
        var engine = TrackpadEngine()
        _ = engine.began(1, at: CGPoint(x: 10, y: 10), time: 0)
        XCTAssertEqual(engine.ended(1, at: CGPoint(x: 10, y: 10), time: 0.1), [.click(.left, at: .cursor)])
    }

    func testMovingIsRelativeAndAccelerated() {
        var engine = TrackpadEngine()
        _ = engine.began(1, at: CGPoint(x: 0, y: 0), time: 0)
        let slow = engine.moved([1: CGPoint(x: 10, y: 0)], time: 1)
        guard case .moveBy(let delta)? = slow.first else { return XCTFail("\(slow)") }
        XCTAssertEqual(delta.dx, 10 * TrackpadEngine.gain(forSpeed: 10), accuracy: 0.001)
        XCTAssertLessThan(TrackpadEngine.gain(forSpeed: 10), TrackpadEngine.gain(forSpeed: 2000))
        XCTAssertEqual(engine.ended(1, at: CGPoint(x: 10, y: 0), time: 1.1), [])
    }

    func testHoldThenMoveDrags() {
        var engine = TrackpadEngine()
        _ = engine.began(1, at: .zero, time: 0)
        XCTAssertEqual(engine.tick(time: 0.4), [.press(.left, at: .cursor), .haptic(.grab)])
        XCTAssertEqual(engine.ended(1, at: .zero, time: 0.8), [.release(.left, at: .cursor)])
    }

    func testTwoFingerTapRightClicksAtTheCursor() {
        var engine = TrackpadEngine()
        _ = engine.began(1, at: CGPoint(x: 0, y: 0), time: 0)
        _ = engine.began(2, at: CGPoint(x: 50, y: 0), time: 0.02)
        XCTAssertEqual(engine.ended(2, at: CGPoint(x: 50, y: 0), time: 0.1), [.click(.right, at: .cursor), .haptic(.secondary)])
    }
}
