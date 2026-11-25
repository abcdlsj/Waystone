import CoreMedia
import Foundation
import VideoToolbox

/// NAL unit rules that differ between the two codecs.
enum NalSyntax {
    case h264
    case hevc

    init(_ codec: Wire.VideoCodec) {
        self = codec == .h264 ? .h264 : .hevc
    }

    func type(of header: UInt8) -> UInt8 {
        switch self {
        case .hevc: return (header >> 1) & 0x3f
        case .h264: return header & 0x1f
        }
    }

    /// VPS/SPS/PPS, which go into the format description.
    func isParameterSet(_ type: UInt8) -> Bool {
        switch self {
        case .hevc: return (32...34).contains(type)
        case .h264: return type == 7 || type == 8
        }
    }

    /// An IDR/IRAP picture, decodable on its own.
    func isKeyframe(_ type: UInt8) -> Bool {
        switch self {
        case .hevc: return (16...21).contains(type)
        case .h264: return type == 5
        }
    }
}

/// Hardware decoding of Annex-B access units with VideoToolbox.
///
/// Parameter sets come with every keyframe; the format description and the
/// decompression session are rebuilt only when they change. Decoding is
/// asynchronous with at most `maxInFlight` frames outstanding: when the decoder
/// falls behind, non-keyframes are skipped rather than queued, because a
/// backlog turns into latency, not smoothness.
final class VideoDecoder {
    struct Stats {
        var decoded = 0
        var skipped = 0
        var failed = 0
    }

    /// Called on a VideoToolbox thread with each decoded picture.
    var onFrame: ((CVPixelBuffer) -> Void)?
    /// Called on the decode queue when the stream cannot continue without a
    /// keyframe (decode error, missing parameter sets).
    var onKeyframeNeeded: (() -> Void)?

    private let syntax: NalSyntax
    private let queue = DispatchQueue(label: "dev.waystone.decode", qos: .userInteractive)
    private var session: VTDecompressionSession?
    private var format: CMFormatDescription?
    private var parameterSets: [Data] = []

    private let lock = NSLock()
    private var inFlight = 0
    private var stats = Stats()
    private var needsKeyframe = true
    static let maxInFlight = 4

    init(codec: Wire.VideoCodec) {
        syntax = NalSyntax(codec)
    }

    deinit {
        if let session {
            VTDecompressionSessionWaitForAsynchronousFrames(session)
            VTDecompressionSessionInvalidate(session)
        }
    }

    var currentStats: Stats {
        lock.lock()
        defer { lock.unlock() }
        return stats
    }

    /// Queues one access unit. Safe to call from any thread.
    func decode(_ accessUnit: Data, rtpTime: UInt64) {
        queue.async { [self] in decodeOnQueue(accessUnit, rtpTime: rtpTime) }
    }

    /// Tears the session down; pending callbacks complete before it returns.
    func invalidate() {
        queue.sync {
            if let session {
                VTDecompressionSessionWaitForAsynchronousFrames(session)
                VTDecompressionSessionInvalidate(session)
            }
            session = nil
            format = nil
            onFrame = nil
        }
    }

    // MARK: - Decode queue

    private func decodeOnQueue(_ accessUnit: Data, rtpTime: UInt64) {
        var sets: [Data] = []
        var slices: [Data] = []
        var keyframe = false
        for nal in Self.splitAnnexB(accessUnit) {
            let type = syntax.type(of: nal[nal.startIndex])
            if syntax.isParameterSet(type) {
                sets.append(nal)
            } else {
                slices.append(nal)
                keyframe = keyframe || syntax.isKeyframe(type)
            }
        }

        if !sets.isEmpty, sets != parameterSets || session == nil {
            guard rebuild(with: sets) else {
                onKeyframeNeeded?()
                return
            }
        }
        guard let session, let format, !slices.isEmpty else { return }

        lock.lock()
        if needsKeyframe {
            guard keyframe else {
                lock.unlock()
                return
            }
            needsKeyframe = false
        }
        if !keyframe && inFlight >= Self.maxInFlight {
            stats.skipped += 1
            // Skipping a P-frame breaks the reference chain for an infinite-GOP
            // stream, so drop subsequent P-frames until a fresh keyframe lands.
            needsKeyframe = true
            lock.unlock()
            onKeyframeNeeded?()
            return
        }
        inFlight += 1
        lock.unlock()

        guard let sample = Self.sampleBuffer(slices, format: format, rtpTime: rtpTime) else {
            finished(decoded: false)
            return
        }
        let status = VTDecompressionSessionDecodeFrame(
            session, sampleBuffer: sample, flags: [._EnableAsynchronousDecompression],
            frameRefcon: nil, infoFlagsOut: nil)
        if status != noErr {
            Log.media.error("decode failed: \(status, privacy: .public)")
            finished(decoded: false)
            markKeyframeNeeded()
        }
    }

    private func rebuild(with sets: [Data]) -> Bool {
        if let session {
            VTDecompressionSessionWaitForAsynchronousFrames(session)
            VTDecompressionSessionInvalidate(session)
        }
        session = nil
        format = nil
        guard let newFormat = makeFormat(sets) else { return false }

        let attributes: [CFString: Any] = [
            kCVPixelBufferPixelFormatTypeKey: kCVPixelFormatType_32BGRA,
            kCVPixelBufferMetalCompatibilityKey: true,
            kCVPixelBufferIOSurfacePropertiesKey: [:] as CFDictionary,
        ]
        var callback = VTDecompressionOutputCallbackRecord(
            decompressionOutputCallback: { refcon, _, status, _, image, _, _ in
                guard let refcon else { return }
                Unmanaged<VideoDecoder>.fromOpaque(refcon).takeUnretainedValue()
                    .output(status: status, image: image)
            },
            decompressionOutputRefCon: Unmanaged.passUnretained(self).toOpaque())
        var created: VTDecompressionSession?
        let status = VTDecompressionSessionCreate(
            allocator: nil, formatDescription: newFormat, decoderSpecification: nil,
            imageBufferAttributes: attributes as CFDictionary, outputCallback: &callback,
            decompressionSessionOut: &created)
        guard status == noErr, let created else {
            Log.media.error("cannot create decoder: \(status, privacy: .public)")
            return false
        }
        VTSessionSetProperty(created, key: kVTDecompressionPropertyKey_RealTime, value: kCFBooleanTrue)
        session = created
        format = newFormat
        parameterSets = sets
        lock.lock()
        needsKeyframe = true
        lock.unlock()
        return true
    }

    private func makeFormat(_ sets: [Data]) -> CMFormatDescription? {
        // The pointers must stay valid for the duration of the call, so the
        // parameter sets are copied into one contiguous buffer first.
        let joined = sets.reduce(Data(), +)
        let sizes = sets.map(\.count)
        var format: CMFormatDescription?
        let status: OSStatus = joined.withUnsafeBytes { raw in
            let base = raw.bindMemory(to: UInt8.self).baseAddress!
            var offsets: [UnsafePointer<UInt8>] = []
            var offset = 0
            for size in sizes {
                offsets.append(base + offset)
                offset += size
            }
            switch syntax {
            case .hevc:
                return CMVideoFormatDescriptionCreateFromHEVCParameterSets(
                    allocator: nil, parameterSetCount: sets.count, parameterSetPointers: offsets,
                    parameterSetSizes: sizes, nalUnitHeaderLength: 4, extensions: nil,
                    formatDescriptionOut: &format)
            case .h264:
                return CMVideoFormatDescriptionCreateFromH264ParameterSets(
                    allocator: nil, parameterSetCount: sets.count, parameterSetPointers: offsets,
                    parameterSetSizes: sizes, nalUnitHeaderLength: 4, formatDescriptionOut: &format)
            }
        }
        if status != noErr { Log.media.error("bad parameter sets: \(status, privacy: .public)") }
        return status == noErr ? format : nil
    }

    // MARK: - Output

    private func output(status: OSStatus, image: CVImageBuffer?) {
        let ok = status == noErr && image != nil
        finished(decoded: ok)
        if ok, let image {
            onFrame?(image)
        } else {
            Log.media.error("async decode failed: \(status, privacy: .public)")
            markKeyframeNeeded()
        }
    }

    private func markKeyframeNeeded() {
        lock.lock()
        let shouldRequest = !needsKeyframe
        needsKeyframe = true
        lock.unlock()
        if shouldRequest {
            onKeyframeNeeded?()
        }
    }

    private func finished(decoded: Bool) {
        lock.lock()
        inFlight = max(0, inFlight - 1)
        if decoded { stats.decoded += 1 } else { stats.failed += 1 }
        lock.unlock()
    }

    // MARK: - Bitstream helpers

    /// Splits Annex-B into NAL units without their start codes.
    static func splitAnnexB(_ data: Data) -> [Data] {
        var units: [Data] = []
        data.withUnsafeBytes { (raw: UnsafeRawBufferPointer) in
            let bytes = raw.bindMemory(to: UInt8.self)
            let count = bytes.count
            var index = 0
            var start: Int?
            while index + 2 < count {
                if bytes[index] == 0, bytes[index + 1] == 0, bytes[index + 2] == 1 {
                    // A 4 byte start code is a 3 byte one preceded by a zero.
                    let end = index > 0 && bytes[index - 1] == 0 ? index - 1 : index
                    if let start, end > start { units.append(data.subdata(in: data.startIndex + start ..< data.startIndex + end)) }
                    index += 3
                    start = index
                } else {
                    index += 1
                }
            }
            if let start, start < count {
                units.append(data.subdata(in: data.startIndex + start ..< data.startIndex + count))
            }
        }
        return units.filter { !$0.isEmpty }
    }

    /// Wraps NAL units as a length-prefixed (AVCC) sample.
    private static func sampleBuffer(_ units: [Data], format: CMFormatDescription, rtpTime: UInt64) -> CMSampleBuffer? {
        var avcc = Data(capacity: units.reduce(0) { $0 + $1.count + 4 })
        for unit in units {
            var length = UInt32(unit.count).bigEndian
            avcc.append(Data(bytes: &length, count: 4))
            avcc.append(unit)
        }
        var block: CMBlockBuffer?
        guard CMBlockBufferCreateWithMemoryBlock(
            allocator: nil, memoryBlock: nil, blockLength: avcc.count, blockAllocator: nil,
            customBlockSource: nil, offsetToData: 0, dataLength: avcc.count,
            flags: kCMBlockBufferAssureMemoryNowFlag, blockBufferOut: &block) == noErr,
            let block
        else { return nil }
        let copied = avcc.withUnsafeBytes { raw in
            CMBlockBufferReplaceDataBytes(
                with: raw.baseAddress!, blockBuffer: block, offsetIntoDestination: 0, dataLength: avcc.count)
        }
        guard copied == noErr else { return nil }

        var timing = CMSampleTimingInfo(
            duration: .invalid, presentationTimeStamp: CMTime(value: Int64(rtpTime), timescale: 90_000),
            decodeTimeStamp: .invalid)
        var size = avcc.count
        var sample: CMSampleBuffer?
        guard CMSampleBufferCreateReady(
            allocator: nil, dataBuffer: block, formatDescription: format, sampleCount: 1,
            sampleTimingEntryCount: 1, sampleTimingArray: &timing, sampleSizeEntryCount: 1,
            sampleSizeArray: &size, sampleBufferOut: &sample) == noErr
        else { return nil }
        return sample
    }
}
