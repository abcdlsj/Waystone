import AVFoundation
import Foundation
import VideoToolbox

/// Hardware HEVC decoding straight into a Metal layer.
///
/// The frames arrive from the Rust transport as Annex-B. VideoToolbox wants AVCC
/// (length prefixed) samples plus a `CMFormatDescription` built from the VPS/SPS/PPS
/// inside the keyframe, so this type converts once per keyframe and then feeds the
/// decoder with `RealTime` enabled and no reordering.
final class VideoDecoder {
    /// Called on the video queue with the newest decoded frame.
    var onFrame: ((CVPixelBuffer) -> Void)?
    /// Reports the decoded size so the UI can match the aspect ratio.
    var onFormatChange: ((CGSize) -> Void)?

    private var session: VTDecompressionSession?
    private var formatDescription: CMFormatDescription?
    private var parameterSets: [[UInt8]] = []
    private let queue = DispatchQueue(label: "dev.waystone.decode", qos: .userInteractive)
    private var awaitingKeyframe = true

    init() {}

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
            let nalType = (first >> 1) & 0x3f
            switch nalType {
            case 32, 33, 34:
                currentSets.append(nal)
            default:
                payloadNals.append(nal)
                if nalType >= 16 && nalType <= 21 {
                    sawKeyframe = true
                }
            }
        }

        if !currentSets.isEmpty,
            formatDescription == nil || currentSets != parameterSets || sawKeyframe {
            if rebuildFormat(with: currentSets) {
                awaitingKeyframe = true
            }
        }

        guard let formatDescription, let session else { return }
        if awaitingKeyframe && !sawKeyframe { return }
        awaitingKeyframe = false

        guard let avcc = Self.toAVCC(payloadNals) else { return }
        guard var sample = Self.makeSampleBuffer(avcc, format: formatDescription, rtpTime: rtpTime) else {
            return
        }
        var flags: VTDecodeInfoFlags = []
        // The output is handled by the session's callback installed at creation
        // time; this synchronous closure only needs to satisfy the API.
        VTDecompressionSessionDecodeFrame(
            session,
            sampleBuffer: sample,
            flags: [._EnableAsynchronousDecompression],
            infoFlagsOut: &flags,
            outputHandler: { _, _, _, _, _ in }
        )
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
                CMVideoFormatDescriptionCreateFromHEVCParameterSets(
                    allocator: nil,
                    parameterSetCount: sets.count,
                    parameterSetPointers: pointerBuffer.baseAddress!,
                    parameterSetSizes: sizeBuffer.baseAddress!,
                    nalUnitHeaderLength: 4,
                    extensions: nil,
                    formatDescriptionOut: &format)
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
                guard status == noErr, let refcon, let imageBuffer else { return }
                let decoder = Unmanaged<VideoDecoder>.fromOpaque(refcon).takeUnretainedValue()
                decoder.handleDecoded(imageBuffer)
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

    private static func dimensions(of format: CMFormatDescription) -> CGSize? {
        let dimensions = CMVideoFormatDescriptionGetDimensions(format)
        guard dimensions.width > 0, dimensions.height > 0 else { return nil }
        return CGSize(width: Int(dimensions.width), height: Int(dimensions.height))
    }

    /// Called by VideoToolbox on a decoder thread.
    fileprivate func handleDecoded(_ imageBuffer: CVImageBuffer) {
        guard let pixelBuffer = imageBuffer as CVPixelBuffer? else { return }
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
        let memory = UnsafeMutableRawPointer.allocate(byteCount: avcc.count, alignment: 1)
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
            memory.deallocate()
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
