import XCTest
@testable import Waystone

final class PictureGeometryTests: XCTestCase {
    /// A 16:10 desktop on a 19.5:9 phone is pillarboxed; taps map onto the
    /// picture, not onto the bars.
    func testPillarboxedPictureMapsTouchesOntoThePicture() {
        var geometry = PictureGeometry()
        geometry.bounds = CGSize(width: 852, height: 393)
        geometry.videoSize = CGSize(width: 1728, height: 1080)
        let rect = geometry.contentRect
        XCTAssertEqual(rect.height, 393, accuracy: 0.01)
        XCTAssertEqual(rect.width, 393 * 1.6, accuracy: 0.01)
        XCTAssertEqual(rect.midX, 426, accuracy: 0.01)

        let topLeft = geometry.normalized(CGPoint(x: rect.minX, y: 0))
        XCTAssertEqual(topLeft.x, 0, accuracy: 1e-6)
        let center = geometry.normalized(CGPoint(x: 426, y: 196.5))
        XCTAssertEqual(center.x, 0.5, accuracy: 1e-6)
        XCTAssertEqual(center.y, 0.5, accuracy: 1e-6)
        // A tap on the black bar clamps to the edge.
        XCTAssertEqual(geometry.normalized(CGPoint(x: 5, y: 100)).x, 0)
    }

    func testZoomKeepsThePointUnderTheFingers() {
        var geometry = PictureGeometry()
        geometry.bounds = CGSize(width: 800, height: 400)
        geometry.videoSize = CGSize(width: 1600, height: 800)
        let focus = CGPoint(x: 600, y: 100)
        let before = geometry.normalized(focus)
        geometry.zoom(by: 2, around: focus, translation: .zero)
        XCTAssertEqual(geometry.scale, 2)
        let after = geometry.normalized(focus)
        XCTAssertEqual(after.x, before.x, accuracy: 1e-6)
        XCTAssertEqual(after.y, before.y, accuracy: 1e-6)
        // Round trip through the inverse mapping.
        let back = geometry.surfacePoint(x: after.x, y: after.y)
        XCTAssertEqual(back.x, focus.x, accuracy: 1e-6)
    }

    func testZoomNeverGoesBelowOneAndPanIsClamped() {
        var geometry = PictureGeometry()
        geometry.bounds = CGSize(width: 800, height: 400)
        geometry.videoSize = CGSize(width: 1600, height: 800)
        geometry.zoom(by: 0.5, around: CGPoint(x: 400, y: 200), translation: CGVector(dx: 500, dy: 0))
        XCTAssertEqual(geometry.scale, 1)
        XCTAssertEqual(geometry.displayedRect, geometry.contentRect)
        geometry.zoom(by: 2, around: CGPoint(x: 400, y: 200), translation: CGVector(dx: 10_000, dy: 0))
        XCTAssertEqual(geometry.displayedRect.minX, 0, accuracy: 1e-6)
    }
}
