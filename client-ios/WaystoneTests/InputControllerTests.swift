import XCTest
@testable import Waystone

@MainActor
final class InputControllerTests: XCTestCase {
    private var sent: [InputEvent] = []

    private func controller(_ mode: InputController.Mode = .direct) -> InputController {
        let input = InputController(mode: mode, haptics: { _ in })
        input.sink = { [unowned self] in self.sent.append($0) }
        input.layout(bounds: CGSize(width: 800, height: 400))
        input.videoSizeChanged(CGSize(width: 1600, height: 800))
        return input
    }

    func testTapBecomesMoveDownUpAtThePicturePosition() {
        let input = controller()
        input.touchBegan(1, at: CGPoint(x: 400, y: 200), time: 0)
        input.touchEnded(1, at: CGPoint(x: 400, y: 200), time: 0.05)
        XCTAssertEqual(sent, [
            .mouseMove(x: 0.5, y: 0.5),
            .mouseButton(.left, down: true, x: 0.5, y: 0.5),
            .mouseButton(.left, down: false, x: 0.5, y: 0.5),
        ])
    }

    func testToggleLatchesAndReleaseAllClearsIt() {
        let input = controller()
        input.perform(.toggle(.leftControl), down: true)
        XCTAssertTrue(input.latchedKeys.contains(.leftControl))
        XCTAssertEqual(sent.last, .key(.leftControl, down: true))
        input.releaseAll()
        XCTAssertTrue(input.latchedKeys.isEmpty)
        XCTAssertEqual(sent.last, .releaseAll)
    }

    func testArmedControlTurnsTypedLetterIntoAShortcut() {
        let input = controller()
        input.toggleArmed(.control)
        input.type("c")
        XCTAssertEqual(sent, [
            .key(.leftControl, down: true),
            .key(KeyCode.letter("c")!, down: true, modifiers: .control),
            .key(KeyCode.letter("c")!, down: false, modifiers: .control),
            .key(.leftControl, down: false),
        ])
        XCTAssertTrue(input.armedModifiers.isEmpty)
        sent.removeAll()
        input.type("hi")
        XCTAssertEqual(sent, [.text("hi")])
    }

    func testJoystickHoldsOnlyTheCurrentDirection() {
        let input = controller()
        let w = KeyCode.letter("w")!, d = KeyCode.letter("d")!
        let all: Set<KeyCode> = [w, d]
        input.hold([w, d], among: all)
        sent.removeAll()
        input.hold([d], among: all)
        XCTAssertEqual(sent, [.key(w, down: false)])
    }

    func testTrackpadMovesTheCursorRelatively() {
        let input = controller(.trackpad)
        input.touchBegan(1, at: CGPoint(x: 100, y: 100), time: 0)
        input.touchesMoved([1: CGPoint(x: 120, y: 100)], time: 10)
        guard case .mouseMove(let x, let y)? = sent.last else { return XCTFail("\(sent)") }
        XCTAssertGreaterThan(x, 0.5)
        XCTAssertEqual(y, 0.5, accuracy: 1e-9)
    }
}
