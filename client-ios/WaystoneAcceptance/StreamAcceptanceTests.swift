import XCTest

/// Drives the real app against a running server and host (docs/ACCEPTANCE.md).
///
/// It is not a unit test: it needs `waystoned` and `waystone-host run` up, and
/// a fresh controller pairing link in `TEST_RUNNER_WAYSTONE_PAIR_URI` (used
/// only when the app has no server yet). Screenshots go to
/// `TEST_RUNNER_WAYSTONE_SHOTS` when set. The host's debug log is the other
/// half of the evidence: every gesture here must show up there as input.
final class StreamAcceptanceTests: XCTestCase {
    private var app: XCUIApplication!
    private var shots: URL?

    override func setUp() {
        continueAfterFailure = false
        let environment = ProcessInfo.processInfo.environment
        shots = environment["WAYSTONE_SHOTS"].map { URL(fileURLWithPath: $0) }
        app = XCUIApplication()
        if let link = environment["WAYSTONE_PAIR_URI"] {
            app.launchEnvironment["WAYSTONE_PAIR_URI"] = link
        }
        app.launch()
    }

    func testPairStreamDriveAndHangUp() throws {
        let hostName = ProcessInfo.processInfo.environment["WAYSTONE_HOST_NAME"] ?? "QA Mac"
        let host = app.buttons.matching(NSPredicate(format: "label BEGINSWITH %@", hostName)).firstMatch
        XCTAssertTrue(host.waitForExistence(timeout: 20), "the host never showed up on the home screen")
        XCTAssertTrue(waitUntil(20) { host.isEnabled }, "the host stayed unavailable")
        shoot("01-home")

        host.tap()
        // The stream is live once the connecting overlay goes away.
        let connecting = app.descendants(matching: .any)["connecting"]
        XCTAssertTrue(connecting.waitForExistence(timeout: 5), "tapping the host did not open the stream")
        let live = waitUntil(30) { !connecting.exists }
        if !live { print(app.debugDescription) }
        XCTAssertTrue(live, "no picture within 30 s")
        // Preferences persist, so an aborted earlier run may have left another
        // mode or preset behind.
        openPanel()
        app.buttons["直点"].tap()
        app.buttons["卡牌"].tap()
        closePanel()
        sleep(2)
        shoot("02-stream-cards")

        // Design doc 5.2: click, hover, hold-and-drag, right click. XCUITest has no
        // two-finger drag, so scrolling is covered by the unit tests only.
        let center = point(0.5, 0.5)
        center.tap()
        point(0.3, 0.5).press(forDuration: 0, thenDragTo: point(0.6, 0.5), withVelocity: .slow, thenHoldForDuration: 0)
        point(0.4, 0.4).press(forDuration: 0.4, thenDragTo: point(0.55, 0.6))
        app.descendants(matching: .any)["stream-surface"].firstMatch.twoFingerTap()
        sleep(1)

        // A capsule from the card preset.
        app.buttons["结束回合"].tap()

        openPanel()
        shoot("03-panel")
        app.buttons["视觉小说"].tap()
        closePanel()
        // Trailing edge zone sends Enter; the floating ball sits on it at mid
        // height. The simulator delivers no touches in the leading ~40 pt in
        // landscape, so the leading zone is not exercised here.
        point(0.97, 0.2).tap()
        app.buttons["快进"].tap()  // latches Ctrl
        sleep(1)
        shoot("04-novel")
        app.buttons["快进"].tap()

        openPanel()
        app.buttons["日常"].tap()
        closePanel()
        sleep(1)
        shoot("05-routine")
        let stick = app.descendants(matching: .any)["方向摇杆"]
        let knob = stick.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.5))
        knob.press(forDuration: 0.3, thenDragTo: knob.withOffset(CGVector(dx: 0, dy: -50)))

        openPanel()
        app.buttons["触控板"].tap()
        closePanel()
        point(0.5, 0.5).press(forDuration: 0, thenDragTo: point(0.7, 0.4), withVelocity: .default, thenHoldForDuration: 0)
        point(0.5, 0.5).tap()
        sleep(1)
        shoot("06-trackpad")

        openPanel()
        app.buttons["直点"].tap()
        app.buttons["卡牌"].tap()
        app.buttons["仅断开，不锁屏"].tap()
        XCTAssertTrue(host.waitForExistence(timeout: 10), "hanging up did not return home")
        shoot("07-home-after")
    }

    // MARK: - Helpers

    private func point(_ x: CGFloat, _ y: CGFloat) -> XCUICoordinate {
        app.coordinate(withNormalizedOffset: CGVector(dx: x, dy: y))
    }

    private func openPanel() {
        let ball = app.descendants(matching: .any)["串流菜单"].firstMatch
        XCTAssertTrue(ball.waitForExistence(timeout: 5))
        ball.tap()
        XCTAssertTrue(app.buttons["断开并锁屏"].waitForExistence(timeout: 5))
    }

    private func closePanel() {
        app.buttons["收起"].tap()
        _ = waitUntil(3) { !self.app.buttons["断开并锁屏"].exists }
    }

    private func shoot(_ name: String) {
        let shot = XCUIScreen.main.screenshot()
        let attachment = XCTAttachment(screenshot: shot)
        attachment.name = name
        attachment.lifetime = .keepAlways
        add(attachment)
        if let shots {
            try? shot.pngRepresentation.write(to: shots.appendingPathComponent("\(name).png"))
        }
    }

    private func waitUntil(_ seconds: TimeInterval, _ condition: () -> Bool) -> Bool {
        let deadline = Date().addingTimeInterval(seconds)
        while Date() < deadline {
            if condition() { return true }
            RunLoop.current.run(until: Date().addingTimeInterval(0.25))
        }
        return condition()
    }
}
