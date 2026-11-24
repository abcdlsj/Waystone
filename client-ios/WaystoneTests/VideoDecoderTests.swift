import XCTest
@testable import Waystone

/// Decoding against a real H.264 keyframe (SPS, PPS, SEI, IDR) from the
/// macOS host's VideoToolbox encoder.
final class VideoDecoderTests: XCTestCase {
    private func fixture(_ name: String) throws -> Data {
        let url = try XCTUnwrap(Bundle(for: Self.self).url(forResource: name, withExtension: nil))
        return try Data(contentsOf: url)
    }

    func testH264KeyframeDecodesToTheEncodedSize() throws {
        let decoder = VideoDecoder(codec: .h264)
        let decoded = expectation(description: "first decoded frame")
        var size = CGSize.zero
        decoder.onFrame = { buffer in
            size = CGSize(width: CVPixelBufferGetWidth(buffer), height: CVPixelBufferGetHeight(buffer))
            decoded.fulfill()
        }
        decoder.decode(try fixture("h264-keyframe.h264"), rtpTime: 0)
        wait(for: [decoded], timeout: 5)
        XCTAssertEqual(size, CGSize(width: 320, height: 240))
        XCTAssertEqual(decoder.currentStats.decoded, 1)
    }

    func testAnnexBSplitHandlesThreeAndFourByteStartCodes() {
        let stream = Data([0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 4, 5])
        XCTAssertEqual(VideoDecoder.splitAnnexB(stream), [Data([0x67, 1, 2]), Data([0x68, 3]), Data([0x65, 4, 5])])
    }

    func testNalTypesPerCodec() {
        // 0x27 is an H.264 SPS but an HEVC prefix SEI.
        XCTAssertEqual(NalSyntax.h264.type(of: 0x27), 7)
        XCTAssertEqual(NalSyntax.hevc.type(of: 0x27), 19)
        XCTAssertTrue(NalSyntax.hevc.isParameterSet(33))
        XCTAssertTrue(NalSyntax.hevc.isKeyframe(19))
    }
}
