import AVFoundation
import Foundation
import VideoToolbox

/// Hardware decoding straight into a Metal layer.
///
/// The frames arrive from the Rust transport as Annex-B. VideoToolbox wants AVCC
/// (length prefixed) samples plus a `CMFormatDescription` built from the
/// parameter sets inside the keyframe, so this type converts once per parameter
/// set change and then feeds the decoder with `RealTime` enabled and no
/// reordering.
///
/// The codec has to be known up front: HEVC and H.264 differ in how NAL types
/// are read from the first byte and in how the format description is built, and
/// guessing wrong drops every frame on the floor.
enum DecodeCodec {
    case hevc
    case h264

    init(name: String) {
        switch name.lowercased() {
        case "h264", "avc": self = .h264
        default: self = .hevc
        }
    }

    /// NAL type of a unit, read from its first header byte.
    func nalType(_ header: UInt8) -> UInt8 {
        switch self {
        case .hevc: return (header >> 1) & 0x3f
        case .h264: return header & 0x1f
        }
    }

    /// Parameter set units, which belong in the format description.
    func isParameterSet(_ nalType: UInt8) -> Bool {
        switch self {
        case .hevc: return (32...34).contains(nalType)
        case .h264: return nalType == 7 || nalType == 8
        }
    }

    /// True when the unit starts an independently decodable picture.
    func isKeyframe(_ nalType: UInt8) -> Bool {
        switch self {
        case .hevc: return (16...21).contains(nalType)
        case .h264: return nalType == 5
        }
    }
}

final class VideoDecoder {
    /// Called on the video queue with the newest decoded frame.
    var onFrame: ((CVPixelBuffer) -> Void)?
    /// Reports the decoded size so the UI can match the aspect ratio.
    var onFormatChange: ((CGSize) -> Void)?

    private let codec: DecodeCodec
    private var session: VTDecompressionSession?
    private var formatDescription: CMFormatDescription?
    private var parameterSets: [[UInt8]] = []
    private let queue = DispatchQueue(label: "dev.waystone.decode", qos: .userInteractive)
    private var awaitingKeyframe = true
    /// Frames handed to VideoToolbox and not answered yet. Bounding this keeps
    /// the pipeline honest: a decoder that falls behind would otherwise queue
    /// every frame and turn latency into seconds of lag.
    private let stateLock = NSLock()
    private var outstanding = 0
    private var decodedFrames = 0
    private var droppedFrames = 0

    /// Decoded frames so far.
    var decodedCount: Int {
        stateLock.lock()
        defer { stateLock.unlock() }
        return decodedFrames
    }

    /// Frames skipped because the decoder was still busy.
    var droppedCount: Int {
        stateLock.lock()
        defer { stateLock.unlock() }
        return droppedFrames
    }

    /// How many frames may be in flight before non-keyframes are skipped.
    private static let maximumOutstanding = 2

    init(codec: DecodeCodec) {
        self.codec = codec
    }

    deinit {
        if let session {
            VTDecompressionSessionInvalidate(session)
        }
    }

    /// Decodes one Annex-B access unit.
    func decode(annexB: [UInt8], rtpTime: UInt64) {
        queue.async { [weak self] in
            self?.decodeLocked(annexB: annexB, rtpTime: rtpTime)
        }
    }

    /// Forces the next frame to be a keyframe.
    func reset() {
        queue.async { [weak self] in
            self?.awaitingKeyframe = true
        }
    }

    private func decodeLocked(annexB: [UInt8], rtpTime: UInt64) {
        let nalUnits = Self.splitAnnexB(annexB)
        guard !nalUnits.isEmpty else { return }

        // Parameter sets arrive with every keyframe; rebuild the format when they
        // change so a host restart with a new resolution keeps working.
        var sawKeyframe = false
        var currentSets: [[UInt8]] = []
        var payloadNals: [[UInt8]] = []
        for nal in nalUnits {
            guard let first = nal.first else { continue }
            let nalType = codec.nalType(first)
            if codec.isParameterSet(nalType) {
                currentSets.append(nal)
            } else {
                payloadNals.append(nal)
                if codec.isKeyframe(nalType) {
                    sawKeyframe = true
                }
            }
        }

        // Rebuilding the format invalidates the decompression session, so it only
        // happens when the parameter sets actually change: a repeat keyframe with
        // identical sets decodes straight away.
        if !currentSets.isEmpty, formatDescription == nil || currentSets != parameterSets {
            guard rebuildFormat(with: currentSets) else { return }
            awaitingKeyframe = true
        }

        guard let formatDescription, let session else {
            // Parameter sets arrive with the first keyframe; anything before it
            // cannot be decoded and is dropped instead of being mis-parsed.
            return
        }
        if awaitingKeyframe && !sawKeyframe { return }
        awaitingKeyframe = false

        // Only the newest frame matters: decoding a backlog adds latency instead
        // of smoothing playback. Keyframes always go through, because skipping
        // one costs the picture until the next one arrives.
        stateLock.lock()
        if !sawKeyframe && outstanding >= Self.maximumOutstanding {
            droppedFrames += 1
            stateLock.unlock()
            return
        }
        outstanding += 1
        stateLock.unlock()

        guard let avcc = Self.toAVCC(payloadNals) else {
            stateLock.lock()
            outstanding = max(0, outstanding - 1)
            stateLock.unlock()
            return
        }
        guard var sample = Self.makeSampleBuffer(avcc, format: formatDescription, rtpTime: rtpTime) else {
            return
        }
        var flags: VTDecodeInfoFlags = []
        // The session was created with an output callback record, so the decode
        // call must not pass a per-frame handler: VideoToolbox answers that
        // combination with `kVTParameterErr` (-12902) and never decodes anything.
        let decodeStatus = VTDecompressionSessionDecodeFrame(
            session,
            sampleBuffer: sample,
            flags: [._EnableAsynchronousDecompression],
            frameRefcon: nil,
            infoFlagsOut: &flags
        )
        #if DEBUG
        if decodeStatus != noErr {
            NSLog("waystone: decode failed with status \(decodeStatus)")
        }
        #endif
        if decodeStatus != noErr {
            stateLock.lock()
            outstanding = max(0, outstanding - 1)
            stateLock.unlock()
        }
    }

    private func rebuildFormat(with sets: [[UInt8]]) -> Bool {
        if let session {
            VTDecompressionSessionInvalidate(session)
            self.session = nil
        }
        let pointers: [UnsafePointer<UInt8>] = sets.map { $0.withUnsafeBufferPointer { $0.baseAddress! } }
        let sizes: [Int] = sets.map { $0.count }
        var format: CMFormatDescription?
        let status = pointers.withUnsafeBufferPointer { pointerBuffer in
            sizes.withUnsafeBufferPointer { sizeBuffer in
                createFormat(
                    count: sets.count,
                    pointers: pointerBuffer.baseAddress!,
                    sizes: sizeBuffer.baseAddress!,
                    format: &format)
            }
        }
        guard status == noErr, let format else { return false }
        self.formatDescription = format
        self.parameterSets = sets

        if let size = Self.dimensions(of: format) {
            onFormatChange?(size)
        }

        let attributes: [CFString: Any] = [
            kCVPixelBufferPixelFormatTypeKey: kCVPixelFormatType_32BGRA,
            kCVPixelBufferMetalCompatibilityKey: true,
            kCVPixelBufferIOSurfacePropertiesKey: [:] as CFDictionary,
        ]
        // The decoder hands finished frames back through this callback, which is
        // where they go straight to the Metal view.
        let outputCallback: VTDecompressionOutputCallbackRecord = VTDecompressionOutputCallbackRecord(
            decompressionOutputCallback: { refcon, _, status, _, imageBuffer, _, _ in
                guard let refcon else { return }
                let decoder = Unmanaged<VideoDecoder>.fromOpaque(refcon).takeUnretainedValue()
                decoder.handleDecoded(status: status, imageBuffer: imageBuffer)
            },
            decompressionOutputRefCon: Unmanaged.passUnretained(self).toOpaque()
        )
        var callbackRecord = outputCallback
        var session: VTDecompressionSession?
        let created = VTDecompressionSessionCreate(
            allocator: nil,
            formatDescription: format,
            decoderSpecification: [kVTVideoDecoderSpecification_EnableHardwareAcceleratedVideoDecoder: true] as CFDictionary,
            imageBufferAttributes: attributes as CFDictionary,
            outputCallback: &callbackRecord,
            decompressionSessionOut: &session)
        guard created == noErr, let session else { return false }
        VTSessionSetProperty(session, key: kVTDecompressionPropertyKey_RealTime, value: kCFBooleanTrue)
        #if os(iOS)
        VTSessionSetProperty(session, key: kVTDecompressionPropertyKey_RequestedMVHEVCVideoLayerIDs, value: nil)
        #endif
        self.session = session
        return true
    }

    private func createFormat(
        count: Int,
        pointers: UnsafePointer<UnsafePointer<UInt8>>,
        sizes: UnsafePointer<Int>,
        format: inout CMFormatDescription?
    ) -> OSStatus {
        switch codec {
        case .hevc:
            return CMVideoFormatDescriptionCreateFromHEVCParameterSets(
                allocator: nil,
                parameterSetCount: count,
                parameterSetPointers: pointers,
                parameterSetSizes: sizes,
                nalUnitHeaderLength: 4,
                extensions: nil,
                formatDescriptionOut: &format)
        case .h264:
            return CMVideoFormatDescriptionCreateFromH264ParameterSets(
                allocator: nil,
                parameterSetCount: count,
                parameterSetPointers: pointers,
                parameterSetSizes: sizes,
                nalUnitHeaderLength: 4,
                formatDescriptionOut: &format)
        }
    }

    private static func dimensions(of format: CMFormatDescription) -> CGSize? {
        let dimensions = CMVideoFormatDescriptionGetDimensions(format)
        guard dimensions.width > 0, dimensions.height > 0 else { return nil }
        return CGSize(width: Int(dimensions.width), height: Int(dimensions.height))
    }

    /// Called by VideoToolbox on a decoder thread, for every frame it finishes
    /// with, including the ones it failed to decode.
    fileprivate func handleDecoded(status: OSStatus, imageBuffer: CVImageBuffer?) {
        stateLock.lock()
        outstanding = max(0, outstanding - 1)
        if status == noErr, imageBuffer != nil {
            decodedFrames += 1
        }
        stateLock.unlock()

        guard status == noErr, let pixelBuffer = imageBuffer as CVPixelBuffer? else { return }
        onFrame?(pixelBuffer)
    }

    /// Splits an Annex-B access unit into its NAL units (start codes removed).
    static func splitAnnexB(_ bytes: [UInt8]) -> [[UInt8]] {
        var units: [[UInt8]] = []
        var index = 0
        var start: Int?
        while index + 2 < bytes.count {
            if bytes[index] == 0, bytes[index + 1] == 0, bytes[index + 2] == 1 {
                if let current = start {
                    units.append(Array(bytes[current..<index]))
                }
                index += 3
                start = index
                continue
            }
            if index + 3 < bytes.count, bytes[index] == 0, bytes[index + 1] == 0,
                bytes[index + 2] == 0, bytes[index + 3] == 1 {
                if let current = start {
                    units.append(Array(bytes[current..<index]))
                }
                index += 4
                start = index
                continue
            }
            index += 1
        }
        if let current = start, current < bytes.count {
            units.append(Array(bytes[current...]))
        }
        return units.filter { !$0.isEmpty }
    }

    /// Converts NAL units into a length prefixed AVCC sample.
    static func toAVCC(_ units: [[UInt8]]) -> [UInt8]? {
        guard !units.isEmpty else { return nil }
        var output: [UInt8] = []
        output.reserveCapacity(units.reduce(0) { $0 + $1.count + 4 })
        for unit in units {
            let size = UInt32(unit.count)
            output.append(UInt8((size >> 24) & 0xff))
            output.append(UInt8((size >> 16) & 0xff))
            output.append(UInt8((size >> 8) & 0xff))
            output.append(UInt8(size & 0xff))
            output.append(contentsOf: unit)
        }
        return output
    }

    private static func makeSampleBuffer(
        _ avcc: [UInt8],
        format: CMFormatDescription,
        rtpTime: UInt64
    ) -> CMSampleBuffer? {
        var blockBuffer: CMBlockBuffer?
        // The block buffer takes ownership and frees the memory with `free`,
        // so the copy has to come from malloc rather than Swift's allocator.
        guard let memory = malloc(avcc.count) else { return nil }
        avcc.withUnsafeBytes { raw in
            memory.copyMemory(from: raw.baseAddress!, byteCount: avcc.count)
        }
        let created = CMBlockBufferCreateWithMemoryBlock(
            allocator: nil,
            memoryBlock: memory,
            blockLength: avcc.count,
            blockAllocator: kCFAllocatorMalloc,
            customBlockSource: nil,
            offsetToData: 0,
            dataLength: avcc.count,
            flags: 0,
            blockBufferOut: &blockBuffer)
        guard created == noErr, let blockBuffer else {
            free(memory)
            return nil
        }

        var sample: CMSampleBuffer?
        var timing = CMSampleTimingInfo(
            duration: .invalid,
            presentationTimeStamp: CMTime(value: Int64(rtpTime), timescale: 90_000),
            decodeTimeStamp: .invalid
        )
        var size = avcc.count
        let status = CMSampleBufferCreateReady(
            allocator: nil,
            dataBuffer: blockBuffer,
            formatDescription: format,
            sampleCount: 1,
            sampleTimingEntryCount: 1,
            sampleTimingArray: &timing,
            sampleSizeEntryCount: 1,
            sampleSizeArray: &size,
            sampleBufferOut: &sample)
        guard status == noErr else { return nil }
        return sample
    }
}
