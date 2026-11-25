// VideoToolbox encoding configured for latency rather than file size.

import CoreMedia
import Foundation
import VideoToolbox

/// One converted frame, ready to be written into RTP.
struct EncodedFrame {
    let data: [UInt8]
    let keyframe: Bool
    /// Presentation time on the host clock, in microseconds.
    let micros: UInt64
}

/// Which bitstream the encoder produces.
///
/// HEVC is the default because every supported iPhone, iPad and Mac decodes it in
/// hardware at roughly half the bitrate of H.264. H.264 exists for the
/// environments without an HEVC decoder (the iOS simulator is the one that
/// matters in practice), and the controller picks the codec per session because
/// only it knows what it can decode.
enum EncodeCodec {
    case hevc
    case h264

    var codecType: CMVideoCodecType {
        switch self {
        case .hevc: return kCMVideoCodecType_HEVC
        case .h264: return kCMVideoCodecType_H264
        }
    }

    var profileLevel: CFString {
        switch self {
        case .hevc: return kVTProfileLevel_HEVC_Main_AutoLevel
        case .h264: return kVTProfileLevel_H264_High_AutoLevel
        }
    }

    /// NAL type of the access unit, read from its first byte.
    func nalType(_ header: UInt8) -> Int {
        switch self {
        case .hevc: return Int((header >> 1) & 0x3f)
        case .h264: return Int(header & 0x1f)
        }
    }

    /// True when a NAL type starts an independently decodable picture.
    func isKeyframeNal(_ nalType: Int) -> Bool {
        switch self {
        case .hevc:
            // IRAP types: BLA 16-18, IDR 19-20, CRA 21.
            return (16...21).contains(nalType)
        case .h264:
            // IDR slice.
            return nalType == 5
        }
    }
}

/// VideoToolbox encoder configured for latency, not for file size.
final class VideoEncoder {
    private var session: VTCompressionSession?
    private let codec: EncodeCodec
    private let width: Int
    private let height: Int
    private let onFrame: (EncodedFrame) -> Void
    private var parameterSets: [[UInt8]] = []
    /// Frames written to `WAYSTONE_DUMP_FRAMES`, also bounded.
    private var dumped = 0

    /// Set by `forceNextKeyframe()`, consumed by the next encode call. The
    /// first frame is always an IDR so the viewer can start decoding at once.
    private var forceKeyframe = true

    /// Not thread safe: the capture session calls it from one serial queue.
    init(
        codec: EncodeCodec, width: Int, height: Int, fps: Int, bitrate: Int32,
        onFrame: @escaping (EncodedFrame) -> Void
    ) throws {
        self.codec = codec
        self.width = width
        self.height = height
        self.onFrame = onFrame
        try prepare(fps: fps, bitrate: bitrate)
    }

    private func prepare(fps: Int, bitrate: Int32) throws {
        let specification: [CFString: Any] = [
            kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder: true,
            kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder: false,
        ]
        let attributes: [CFString: Any] = [
            kCVPixelBufferPixelFormatTypeKey: kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
            kCVPixelBufferIOSurfacePropertiesKey: [:] as CFDictionary,
        ]
        var session: VTCompressionSession?
        let status = VTCompressionSessionCreate(
            allocator: nil,
            width: Int32(width),
            height: Int32(height),
            codecType: codec.codecType,
            encoderSpecification: specification as CFDictionary,
            imageBufferAttributes: attributes as CFDictionary,
            compressedDataAllocator: nil,
            outputCallback: nil,
            refcon: nil,
            compressionSessionOut: &session
        )
        guard status == noErr, let session else {
            throw CaptureError.encoderUnavailable(status)
        }
        self.session = session

        // Latency settings: no reordering, no look-ahead, unlimited GOP with
        // keyframes only on request.
        setProperty(kVTCompressionPropertyKey_RealTime, kCFBooleanTrue)
        setProperty(kVTCompressionPropertyKey_AllowFrameReordering, kCFBooleanFalse)
        setProperty(kVTCompressionPropertyKey_ProfileLevel, codec.profileLevel)
        setProperty(kVTCompressionPropertyKey_MaxKeyFrameInterval, 0 as CFNumber)
        setProperty(kVTCompressionPropertyKey_ExpectedFrameRate, fps as CFNumber)
        setBitrate(bitrate)
        // Deliberately left at the default: the picture is mostly text, and
        // trading quality for encoder speed smears glyph edges, which on a phone
        // is the first thing the eye notices.
        VTCompressionSessionPrepareToEncodeFrames(session)
    }

    /// Sets the average rate and a hard one-second cap of 1.5 times it, so a
    /// keyframe cannot burst far past what the path was measured to carry.
    func setBitrate(_ bitrate: Int32) {
        setProperty(kVTCompressionPropertyKey_AverageBitRate, bitrate as CFNumber)
        let bytesPerSecond = Double(bitrate) * 1.5 / 8
        setProperty(kVTCompressionPropertyKey_DataRateLimits, [bytesPerSecond, 1.0] as CFArray)
    }

    func setFrameRate(_ fps: Int) {
        setProperty(kVTCompressionPropertyKey_ExpectedFrameRate, fps as CFNumber)
    }

    /// VideoToolbox has no "emit a keyframe now" call; the request is a frame
    /// property, so the flag is consumed by the next `encode`.
    func forceNextKeyframe() {
        forceKeyframe = true
    }

    /// Encodes one frame, optionally forcing an IDR.
    func encode(_ buffer: CVPixelBuffer, presentation: CMTime, forceKeyframe: Bool = false) {
        guard let session else { return }
        var properties: CFDictionary? = nil
        if forceKeyframe || self.forceKeyframe {
            self.forceKeyframe = false
            properties = [
                kVTEncodeFrameOptionKey_ForceKeyFrame: true
            ] as CFDictionary
        }
        VTCompressionSessionEncodeFrame(
            session,
            imageBuffer: buffer,
            presentationTimeStamp: presentation,
            duration: .invalid,
            frameProperties: properties,
            infoFlagsOut: nil
        ) { [weak self] status, flags, sampleBuffer in
            guard let self, status == noErr, let sampleBuffer else { return }
            let keyframe = !flags.contains(.frameDropped) && self.isKeyframe(sampleBuffer)
            let micros = UInt64(max(0, CMTimeGetSeconds(presentation)) * 1_000_000)
            guard let annexB = self.toAnnexB(sampleBuffer, keyframe: keyframe) else { return }
            // Development aid: capture the exact bitstream the engine sends, so a
            // decode failure can be reproduced outside the app.
            if let dir = ProcessInfo.processInfo.environment["WAYSTONE_DUMP_FRAMES"],
                self.dumped < 40
            {
                self.dumped += 1
                let suffix = self.codec == .h264 ? "h264" : "h265"
                let name = String(format: "frame-%03d-%@.%@", self.dumped, keyframe ? "k" : "p", suffix)
                try? Data(annexB).write(to: URL(fileURLWithPath: dir).appendingPathComponent(name))
            }
            self.onFrame(EncodedFrame(data: annexB, keyframe: keyframe, micros: micros))
        }
    }

    func finish() {
        guard let session else { return }
        VTCompressionSessionCompleteFrames(session, untilPresentationTimeStamp: .invalid)
        VTCompressionSessionInvalidate(session)
        self.session = nil
    }

    private func setProperty(_ key: CFString, _ value: CFTypeRef?) {
        guard let session, let value else { return }
        VTSessionSetProperty(session, key: key, value: value)
    }

    /// True when the access unit contains an IRAP picture (a keyframe).
    ///
    /// The sample attachments are unreliable for HEVC, so the NAL types are the
    /// source of truth: the receiver needs an IDR to start decoding anyway.
    private func isKeyframe(_ sampleBuffer: CMSampleBuffer) -> Bool {
        guard let block = CMSampleBufferGetDataBuffer(sampleBuffer) else { return false }
        var length = 0
        var pointer: UnsafeMutablePointer<Int8>?
        guard
            CMBlockBufferGetDataPointer(
                block, atOffset: 0, lengthAtOffsetOut: nil, totalLengthOut: &length,
                dataPointerOut: &pointer) == noErr,
            let pointer
        else { return false }

        let bytes = UnsafeRawPointer(pointer).assumingMemoryBound(to: UInt8.self)
        var offset = 0
        while offset + 4 <= length {
            let size = Int(bytes[offset]) << 24 | Int(bytes[offset + 1]) << 16
                | Int(bytes[offset + 2]) << 8 | Int(bytes[offset + 3])
            offset += 4
            if size <= 0 || offset + size > length { break }
            if codec.isKeyframeNal(codec.nalType(bytes[offset])) {
                return true
            }
            offset += size
        }
        return false
    }

    /// Converts the length-prefixed AVCC sample into Annex-B, prefixing the
    /// parameter sets on every keyframe so a late joiner can start decoding.
    private func toAnnexB(_ sampleBuffer: CMSampleBuffer, keyframe: Bool) -> [UInt8]? {
        if parameterSets.isEmpty || keyframe {
            parameterSets = readParameterSets(sampleBuffer)
        }
        guard let block = CMSampleBufferGetDataBuffer(sampleBuffer) else { return nil }
        var length = 0
        var pointer: UnsafeMutablePointer<Int8>?
        let status = CMBlockBufferGetDataPointer(
            block, atOffset: 0, lengthAtOffsetOut: nil, totalLengthOut: &length, dataPointerOut: &pointer)
        guard status == noErr, let pointer else { return nil }

        var output = [UInt8]()
        output.reserveCapacity(length + 64)
        if keyframe {
            for set in parameterSets {
                output.append(contentsOf: [0, 0, 0, 1])
                output.append(contentsOf: set)
            }
        }
        var offset = 0
        let bytes = UnsafeRawPointer(pointer).assumingMemoryBound(to: UInt8.self)
        while offset + 4 <= length {
            let size = Int(bytes[offset]) << 24 | Int(bytes[offset + 1]) << 16
                | Int(bytes[offset + 2]) << 8 | Int(bytes[offset + 3])
            offset += 4
            if size <= 0 || offset + size > length { break }
            output.append(contentsOf: [0, 0, 0, 1])
            output.append(contentsOf: UnsafeBufferPointer(start: bytes + offset, count: size))
            offset += size
        }
        return output.isEmpty ? nil : output
    }
    /// Extracts the access unit's parameter sets (VPS/SPS/PPS for HEVC, SPS/PPS
    /// for H.264) from the sample's format description.
    private func readParameterSets(_ sampleBuffer: CMSampleBuffer) -> [[UInt8]] {
        guard let format = CMSampleBufferGetFormatDescription(sampleBuffer) else { return [] }
        switch codec {
        case .hevc: return Self.readHevcParameterSets(format)
        case .h264: return Self.readH264ParameterSets(format)
        }
    }

    /// Extracts VPS, SPS and PPS from an HEVC format description.
    private static func readHevcParameterSets(_ format: CMFormatDescription) -> [[UInt8]] {
        var sets: [[UInt8]] = []
        var count = 0
        var nalLength: Int32 = 0
        if CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(
            format, parameterSetIndex: 0, parameterSetPointerOut: nil,
            parameterSetSizeOut: nil, parameterSetCountOut: &count,
            nalUnitHeaderLengthOut: &nalLength) != noErr {
            return sets
        }
        for index in 0..<count {
            var pointer: UnsafePointer<UInt8>?
            var size = 0
            if CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(
                format, parameterSetIndex: index, parameterSetPointerOut: &pointer,
                parameterSetSizeOut: &size, parameterSetCountOut: nil,
                nalUnitHeaderLengthOut: nil) == noErr, let pointer, size > 0 {
                sets.append(Array(UnsafeBufferPointer(start: pointer, count: size)))
            }
        }
        return sets
    }

    /// Extracts SPS and PPS from an H.264 format description.
    private static func readH264ParameterSets(_ format: CMFormatDescription) -> [[UInt8]] {
        var sets: [[UInt8]] = []
        var count = 0
        var nalLength: Int32 = 0
        if CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
            format, parameterSetIndex: 0, parameterSetPointerOut: nil,
            parameterSetSizeOut: nil, parameterSetCountOut: &count,
            nalUnitHeaderLengthOut: &nalLength) != noErr {
            return sets
        }
        for index in 0..<count {
            var pointer: UnsafePointer<UInt8>?
            var size = 0
            if CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                format, parameterSetIndex: index, parameterSetPointerOut: &pointer,
                parameterSetSizeOut: &size, parameterSetCountOut: nil,
                nalUnitHeaderLengthOut: nil) == noErr, let pointer, size > 0 {
                sets.append(Array(UnsafeBufferPointer(start: pointer, count: size)))
            }
        }
        return sets
    }
}

