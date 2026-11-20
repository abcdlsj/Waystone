import XCTest
@testable import Waystone

/// Decoding regression tests against a real H.264 access unit.
///
/// The fixture is one keyframe (SPS, PPS, SEI, IDR) from a VideoToolbox encoder,
/// the same shape the macOS host sends. It exists because the decoder used to
/// pass a per-frame output handler to a session that already had an output
/// callback, and VideoToolbox answered every frame with `kVTParameterErr`
/// (-12902) instead of a picture.
final class VideoDecoderTests: XCTestCase {
    private func fixture(named name: String) throws -> [UInt8] {
        let url = try XCTUnwrap(Bundle(for: Self.self).url(forResource: name, withExtension: nil))
        return Array(try Data(contentsOf: url))
    }

    func testH264KeyframeDecodesToTheEncodedSize() throws {
        let annexB = try fixture(named: "h264-keyframe.h264")

        let decoder = VideoDecoder(codec: .h264)
        let decoded = expectation(description: "first decoded frame")
        var size = CGSize.zero
        decoder.onFrame = { buffer in
            size = CGSize(
                width: CVPixelBufferGetWidth(buffer), height: CVPixelBufferGetHeight(buffer))
            decoded.fulfill()
        }

        decoder.decode(annexB: annexB, rtpTime: 0)
        wait(for: [decoded], timeout: 5)
        XCTAssertEqual(size, CGSize(width: 320, height: 240))
    }

    func testDecodeCodecReadsNalTypesPerCodec() {
        // 0x27 is an H.264 SPS and an HEVC prefix SEI depending on the codec.
        XCTAssertEqual(DecodeCodec.h264.nalType(0x27), 7)
        XCTAssertEqual(DecodeCodec.hevc.nalType(0x27), 19)
        XCTAssertTrue(DecodeCodec.h264.isParameterSet(7))
        XCTAssertTrue(DecodeCodec.hevc.isParameterSet(33))
        XCTAssertTrue(DecodeCodec.h264.isKeyframe(5))
        XCTAssertTrue(DecodeCodec.hevc.isKeyframe(19))
    }
}
