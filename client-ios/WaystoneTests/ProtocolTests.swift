import XCTest
@testable import Waystone

/// Byte and JSON compatibility with `crates/waystone-proto`.
final class ProtocolTests: XCTestCase {
    func testInputFramesMatchTheRustLayout() {
        XCTAssertEqual([UInt8](InputEvent.mouseMove(x: 0, y: 1).encoded()), [0x01, 0, 0, 0xff, 0xff])
        XCTAssertEqual(
            [UInt8](InputEvent.mouseButton(.right, down: true, x: 0.5, y: 0).encoded()),
            [0x02, 1, 1, 0x80, 0x00, 0, 0])
        XCTAssertEqual([UInt8](InputEvent.wheel(dx: -1, dy: 2).encoded()), [0x03, 0xff, 0xff, 0, 2])
        XCTAssertEqual(
            [UInt8](InputEvent.key(.space, down: true, modifiers: .control).encoded()),
            [0x04, 0, 0, 0, 0x2c, 1, 2])
        XCTAssertEqual([UInt8](InputEvent.text("é").encoded()), [0x05, 0xc3, 0xa9])
        XCTAssertEqual([UInt8](InputEvent.releaseAll.encoded()), [0x06])
        XCTAssertFalse(InputEvent.mouseMove(x: 0, y: 0).isReliable)
        XCTAssertTrue(InputEvent.releaseAll.isReliable)
    }

    func testHidKeyCodes() {
        XCTAssertEqual(KeyCode.letter("A")?.rawValue, 0x04)
        XCTAssertEqual(KeyCode.letter("z")?.rawValue, 0x1d)
        XCTAssertEqual(KeyCode.digit(1)?.rawValue, 0x1e)
        XCTAssertEqual(KeyCode.digit(0)?.rawValue, 0x27)
        XCTAssertEqual(KeyCode.function(1)?.rawValue, 0x3a)
        XCTAssertEqual(KeyCode.function(12)?.rawValue, 0x45)
        XCTAssertNil(KeyCode.letter("1"))
    }

    func testSessionStartJSON() throws {
        let message = Wire.ClientMessage.sessionStart(
            target: "abc", viewport: Wire.Viewport(width: 2556, height: 1179, dpi: 460, fps: 60),
            audio: true, videoCodec: .h264)
        let json = try JSONSerialization.jsonObject(with: Data(message.json().utf8)) as! [String: Any]
        XCTAssertEqual(json["type"] as? String, "session_start")
        XCTAssertEqual(json["video_codec"] as? String, "h264")
        XCTAssertEqual((json["viewport"] as? [String: Any])?["width"] as? Int, 2556)
    }

    func testAuthOkDecodesWhatTheServerSends() throws {
        let text = """
            {"type":"auth_ok","peers":[{"device":{"id":"0123456789abcdef","name":"Studio","platform":"macos",\
            "role":"host","public_key":"AAAA"},"online":true,"rtt_ms":12,"busy":true}],\
            "turn":{"server":"h:3478","username":"u","password":"p","relay_bitrate_ceiling":4000000}}
            """
        guard case .authOk(let peers, let turn) = try Wire.ServerMessage.decode(text) else {
            return XCTFail("wrong message")
        }
        XCTAssertEqual(peers.first?.device.platform, .macos)
        XCTAssertEqual(peers.first?.rttMs, 12)
        XCTAssertEqual(peers.first?.busy, true)
        XCTAssertEqual(turn?.relayBitrateCeiling, 4_000_000)
    }

    func testOlderServersWithoutNewFieldsStillDecode() throws {
        let text = """
            {"type":"peer_update","peers":[{"device":{"id":"x","name":"PC","platform":"beos",\
            "role":"host","public_key":"k"},"online":false,"rtt_ms":null,"hostname":null}]}
            """
        guard case .peerUpdate(let peers) = try Wire.ServerMessage.decode(text) else {
            return XCTFail("wrong message")
        }
        XCTAssertEqual(peers.first?.device.platform, .other)
        XCTAssertEqual(peers.first?.busy, false)
    }

    func testSessionEndedReasons() throws {
        let text = #"{"type":"session_ended","session_id":"s","reason":"peer_offline","detail":null}"#
        guard case .sessionEnded(_, let reason, _) = try Wire.ServerMessage.decode(text) else {
            return XCTFail("wrong message")
        }
        XCTAssertEqual(reason, .peerOffline)
    }

    func testPairingLinkParsesWhatTheServerRenders() {
        let fingerprint = String(repeating: "ab", count: 32)
        let link = PairingLink(
            "waystone://pair?ep=%5B2001%3Adb8%3A%3A1%5D%3A8443&turn=10.0.0.2%3A3478&fp=\(fingerprint)&tk=abcdefghijklmnop")
        XCTAssertEqual(link?.endpoint, "[2001:db8::1]:8443")
        XCTAssertEqual(link?.turn, "10.0.0.2:3478")
        XCTAssertEqual(Instance(link: link!).label, "2001:db8::1")
        XCTAssertNil(PairingLink("waystone://pair?ep=a:1&turn=a:2&fp=short&tk=abcdefghijklmnop"))
        XCTAssertNil(PairingLink("https://example.com"))
    }
}
