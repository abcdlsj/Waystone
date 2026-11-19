// Capture and encode the macOS desktop for Waystone.
//
// This file is compiled by `build.rs` into a static library and called from the
// Rust host agent through the C functions at the bottom. Everything here is on
// the hot path of the stream, so the design is:
//
//   ScreenCaptureKit (IOSurface, NV12)
//     -> VideoToolbox HEVC (no B frames, no look-ahead)
//     -> AVCC to Annex-B conversion
//     -> one C callback per frame
//
// Audio follows the same shape: ScreenCaptureKit delivers 48 kHz PCM, an
// AudioConverter turns it into 20 ms Opus packets.

import AudioToolbox
import CoreMedia
import CoreVideo
import Foundation
import ScreenCaptureKit
import VideoToolbox

/// Event kinds shared with `crates/waystone-rtc/src/ffi.rs`.
private let kindVideo: Int32 = 0
private let kindAudio: Int32 = 1

/// Signature of the callback the Rust side installs.
public typealias CaptureCallback = @convention(c) (
    Int32, UnsafePointer<UInt8>?, Int, Bool, UInt64, UnsafeMutableRawPointer?
) -> Void

/// Holds the pieces that a capture session owns.
final class CaptureSession: NSObject, SCStreamOutput, SCStreamDelegate {
    private let callback: CaptureCallback
    private let context: UnsafeMutableRawPointer?
    private var stream: SCStream?
    private var encoder: HEVCEncoder?
    private var opus: OpusEncoder?
    private var startedAt = Date()
    private let lock = NSLock()

    init(callback: @escaping CaptureCallback, context: UnsafeMutableRawPointer?) {
        self.callback = callback
        self.context = context
    }

    func start(width: Int, height: Int, fps: Int, bitrate: Int32) async throws {
        let content = try await SCShareableContent.excludingDesktopWindows(
            false, onScreenWindowsOnly: false)
        guard let display = content.displays.first else {
            throw CaptureError.noDisplay
        }

        let filter = SCContentFilter(display: display, excludingWindows: [])
        let configuration = SCStreamConfiguration()
        configuration.width = width
        configuration.height = height
        configuration.minimumFrameInterval = CMTime(value: 1, timescale: CMTimeScale(fps))
        configuration.pixelFormat = kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange
        configuration.scalesToFit = true
        configuration.showsCursor = true
        configuration.capturesAudio = true
        configuration.sampleRate = 48_000
        configuration.channelCount = 2
        configuration.excludesCurrentProcessAudio = true
        configuration.queueDepth = 3

        encoder = HEVCEncoder(width: width, height: height, fps: fps, bitrate: bitrate) { [weak self] frame in
            guard let self, let callback = Optional(self.callback) else { return }
            frame.data.withUnsafeBufferPointer { buffer in
                callback(
                    kindVideo,
                    buffer.baseAddress,
                    buffer.count,
                    frame.keyframe,
                    frame.timestampMicros,
                    self.context
                )
            }
        }
        try encoder?.prepare()

        opus = OpusEncoder()
        try opus?.prepare()

        let stream = SCStream(filter: filter, configuration: configuration, delegate: self)
        try stream.addStreamOutput(self, type: .screen, sampleHandlerQueue: videoQueue)
        try stream.addStreamOutput(self, type: .audio, sampleHandlerQueue: audioQueue)
        try await stream.startCapture()
        self.stream = stream
        startedAt = Date()
    }

    // Serial queues keep video and audio independent so a slow encoder never
    // delays the other track.
    private let videoQueue = DispatchQueue(label: "dev.waystone.capture.video", qos: .userInteractive)
    private let audioQueue = DispatchQueue(label: "dev.waystone.capture.audio", qos: .userInteractive)

    func stop() async {
        try? await stream?.stopCapture()
        stream = nil
        encoder?.finish()
        encoder = nil
        opus = nil
    }

    func requestKeyframe() {
        encoder?.requestKeyframe()
    }

    func setBitrate(_ bitrate: Int32) {
        encoder?.setBitrate(bitrate)
    }

    func stream(_ stream: SCStream, didOutputSampleBuffer sampleBuffer: CMSampleBuffer, of type: SCStreamOutputType) {
        switch type {
        case .screen:
            guard let pixelBuffer = sampleBuffer.imageBuffer else { return }
            let presentation = CMSampleBufferGetPresentationTimeStamp(sampleBuffer)
            encoder?.encode(pixelBuffer, presentation: presentation)
        case .audio:
            opus?.encode(sampleBuffer) { [weak self] packet, microseconds in
                guard let self else { return }
                let callback = self.callback
                packet.withUnsafeBufferPointer { buffer in
                    callback(kindAudio, buffer.baseAddress, buffer.count, false, microseconds, self.context)
                }
            }
        default:
            break
        }
    }

    func stream(_ stream: SCStream, didStopWithError error: Error) {
        NSLog("waystone capture stopped: \(error.localizedDescription)")
    }
}

enum CaptureError: Error {
    case noDisplay
    case encoderUnavailable(OSStatus)
}

/// One converted frame, ready to be written into RTP.
struct EncodedFrame {
    let data: [UInt8]
    let keyframe: Bool
    let timestampMicros: UInt64
}

/// VideoToolbox HEVC encoder configured for latency, not for file size.
final class HEVCEncoder {
    private var session: VTCompressionSession?
    private let width: Int
    private let height: Int
    private let onFrame: (EncodedFrame) -> Void
    private var parameterSets: [[UInt8]] = []

    private var bitrate: Int32

    init(width: Int, height: Int, fps: Int, bitrate: Int32, onFrame: @escaping (EncodedFrame) -> Void) {
        self.width = width
        self.height = height
        self.bitrate = bitrate
        self.onFrame = onFrame
        _ = fps
    }

    func prepare() throws {
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
            codecType: kCMVideoCodecType_HEVC,
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
        setProperty(kVTCompressionPropertyKey_ProfileLevel, kVTProfileLevel_HEVC_Main_AutoLevel)
        setProperty(kVTCompressionPropertyKey_MaxKeyFrameInterval, 0 as CFNumber)
        setProperty(kVTCompressionPropertyKey_AverageBitRate, bitrate as CFNumber)
        setProperty(kVTCompressionPropertyKey_ExpectedFrameRate, 60 as CFNumber)
        if #available(macOS 14.0, *) {
            setProperty(kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality, kCFBooleanTrue)
        }
        VTCompressionSessionPrepareToEncodeFrames(session)
    }

    func setBitrate(_ bitrate: Int32) {
        self.bitrate = bitrate
        guard let session else { return }
        VTSessionSetProperty(
            session, key: kVTCompressionPropertyKey_AverageBitRate, value: bitrate as CFNumber)
    }

    func requestKeyframe() {
        guard let session else { return }
        VTCompressionSessionCompleteFrames(session, untilPresentationTimeStamp: .invalid)
    }

    func encode(_ buffer: CVPixelBuffer, presentation: CMTime) {
        guard let session else { return }
        let properties: CFDictionary? = nil
        VTCompressionSessionEncodeFrame(
            session,
            imageBuffer: buffer,
            presentationTimeStamp: presentation,
            duration: .invalid,
            frameProperties: properties,
            infoFlagsOut: nil
        ) { [weak self] status, flags, sampleBuffer in
            guard let self, status == noErr, let sampleBuffer else { return }
            let keyframe = !flags.contains(.frameDropped) && Self.isKeyframe(sampleBuffer)
            let micros = UInt64(max(0, CMTimeGetSeconds(presentation)) * 1_000_000)
            guard let annexB = self.toAnnexB(sampleBuffer, keyframe: keyframe) else { return }
            self.onFrame(EncodedFrame(data: annexB, keyframe: keyframe, timestampMicros: micros))
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

    private static func isKeyframe(_ sampleBuffer: CMSampleBuffer) -> Bool {
        guard
            let attachments = CMSampleBufferGetSampleAttachmentsArray(
                sampleBuffer, createIfNecessary: false) as? [[CFString: Any]],
            let first = attachments.first
        else {
            return true
        }
        let notSync = first[kCMSampleAttachmentKey_NotSync] as? Bool ?? false
        let dependsOnOthers = first[kCMSampleAttachmentKey_DependsOnOthers] as? Bool ?? false
        return !notSync && !dependsOnOthers
    }

    /// Converts the length-prefixed AVCC sample into Annex-B, prefixing the
    /// parameter sets on every keyframe so a late joiner can start decoding.
    private func toAnnexB(_ sampleBuffer: CMSampleBuffer, keyframe: Bool) -> [UInt8]? {
        guard let format = CMSampleBufferGetFormatDescription(sampleBuffer) else { return nil }
        if parameterSets.isEmpty || keyframe {
            parameterSets = Self.readParameterSets(format)
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

    /// Extracts VPS, SPS and PPS from an HEVC format description.
    private static func readParameterSets(_ format: CMFormatDescription) -> [[UInt8]] {
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
}

/// Encodes 48 kHz stereo PCM into 20 ms Opus packets through AudioToolbox.
final class OpusEncoder {
    private var converter: AudioConverterRef?
    private var inputFormat = AudioStreamBasicDescription()
    private var pending: [Float] = []
    private let framesPerPacket: AVAudioFrameCount = 960

    func prepare() throws {
        // AudioToolbox ships an Opus encoder on macOS 11 and later; no external
        // library is needed, which keeps the app store build simple.
    }

    func encode(_ sampleBuffer: CMSampleBuffer, emit: ([UInt8], UInt64) -> Void) {
        guard let description = CMSampleBufferGetFormatDescription(sampleBuffer),
            let asbd = CMAudioFormatDescriptionGetStreamBasicDescription(description)
        else { return }
        let format = asbd.pointee
        if converter == nil {
            guard makeConverter(input: format) else { return }
        }
        guard let converter else { return }

        let count = CMSampleBufferGetNumSamples(sampleBuffer)
        guard count > 0, let block = CMSampleBufferGetDataBuffer(sampleBuffer) else { return }
        var length = 0
        var pointer: UnsafeMutablePointer<Int8>?
        guard CMBlockBufferGetDataPointer(
            block, atOffset: 0, lengthAtOffsetOut: nil, totalLengthOut: &length,
            dataPointerOut: &pointer) == noErr, let pointer else { return }

        // Normalize whatever ScreenCaptureKit handed us into interleaved float32.
        append(samples: UnsafeRawPointer(pointer), length: length, format: format, count: count)

        let timestamp = UInt64(max(0, CMTimeGetSeconds(CMSampleBufferGetPresentationTimeStamp(sampleBuffer))) * 1_000_000)
        while pending.count >= Int(framesPerPacket) * 2 {
            let slice = Array(pending.prefix(Int(framesPerPacket) * 2))
            pending.removeFirst(slice.count)
            if let packet = encodeFrame(converter: converter, samples: slice) {
                emit(packet, timestamp)
            }
        }
    }

    private func makeConverter(input: AudioStreamBasicDescription) -> Bool {
        var source = input
        var destination = AudioStreamBasicDescription(
            mSampleRate: 48_000,
            mFormatID: kAudioFormatOpus,
            mFormatFlags: 0,
            mBytesPerPacket: 0,
            mFramesPerPacket: framesPerPacket,
            mBytesPerFrame: 0,
            mChannelsPerFrame: 2,
            mBitsPerChannel: 0,
            mReserved: 0
        )
        var converter: AudioConverterRef?
        guard AudioConverterNew(&source, &destination, &converter) == noErr, let converter else {
            NSLog("waystone: cannot create the Opus encoder")
            return false
        }
        var bitrate: UInt32 = 96_000
        AudioConverterSetProperty(converter, kAudioConverterEncodeBitRate, 4, &bitrate)
        self.converter = converter
        self.inputFormat = source
        return true
    }

    private func append(samples: UnsafeRawPointer, length: Int, format: AudioStreamBasicDescription, count: Int) {
        let isFloat = format.mFormatFlags & kAudioFormatFlagIsFloat != 0
        let isNonInterleaved = format.mFormatFlags & kAudioFormatFlagIsNonInterleaved != 0
        let channels = Int(format.mChannelsPerFrame)
        guard isFloat, channels == 2 else { return }

        if isNonInterleaved {
            // Two planes of mono float.
            let plane = length / 2
            let left = samples.assumingMemoryBound(to: Float.self)
            let right = UnsafeRawPointer(left).advanced(by: plane).assumingMemoryBound(to: Float.self)
            for index in 0..<(plane / MemoryLayout<Float>.size) {
                pending.append(left[index])
                pending.append(right[index])
            }
        } else {
            let interleaved = samples.assumingMemoryBound(to: Float.self)
            pending.append(contentsOf: UnsafeBufferPointer(start: interleaved, count: count * 2))
        }
    }

    private func encodeFrame(converter: AudioConverterRef, samples: [Float]) -> [UInt8]? {
        // The input proc is a C function pointer, so it cannot close over local
        // variables; the sample buffer is handed over through a box instead.
        final class Box {
            let input: [Float]
            var consumed = false
            init(_ input: [Float]) { self.input = input }
        }
        let box = Box(samples)
        let context = Unmanaged.passUnretained(box).toOpaque()

        var output = [UInt8](repeating: 0, count: 4000)
        var packets: UInt32 = 1
        let bufferList = AudioBufferList.allocate(maximumBuffers: 1)
        defer { free(bufferList.unsafeMutablePointer) }

        let status: OSStatus = output.withUnsafeMutableBytes { raw -> OSStatus in
            bufferList[0] = AudioBuffer(
                mNumberChannels: 2,
                mDataByteSize: UInt32(raw.count),
                mData: raw.baseAddress
            )
            return AudioConverterFillComplexBuffer(
                converter,
                { _, ioPackets, ioData, _, refcon in
                    guard let refcon else {
                        ioPackets.pointee = 0
                        return noErr
                    }
                    let box = Unmanaged<Box>.fromOpaque(refcon).takeUnretainedValue()
                    if box.consumed {
                        ioPackets.pointee = 0
                        return noErr
                    }
                    box.consumed = true
                    ioPackets.pointee = UInt32(box.input.count / 2)
                    let destination = UnsafeMutableAudioBufferListPointer(ioData)
                    box.input.withUnsafeBytes { raw in
                        destination[0].mData = UnsafeMutableRawPointer(mutating: raw.baseAddress)
                        destination[0].mDataByteSize = UInt32(raw.count)
                        destination[0].mNumberChannels = 2
                    }
                    return noErr
                },
                context,
                &packets,
                bufferList.unsafeMutablePointer,
                nil
            )
        }
        guard status == noErr, packets > 0, bufferList[0].mDataByteSize > 0 else { return nil }
        let size = Int(bufferList[0].mDataByteSize)
        guard let pointer = bufferList[0].mData else { return nil }
        return Array(UnsafeBufferPointer(start: pointer.assumingMemoryBound(to: UInt8.self), count: size))
    }
}

// MARK: - C entry points

private final class Slot {
    static var session: CaptureSession?
    static var task: Task<Void, Never>?
}

/// Starts capturing the main display and encoding at `width`x`height`.
///
/// Returns 0 on success, negative on failure. `callback` is invoked from the
/// capture queues; the Rust side forwards the frames into the WebRTC engine.
@_cdecl("ws_capture_start")
public func ws_capture_start(
    _ width: Int32,
    _ height: Int32,
    _ fps: Int32,
    _ bitrate: Int32,
    _ callback: @escaping CaptureCallback,
    _ context: UnsafeMutableRawPointer?
) -> Int32 {
    let session = CaptureSession(callback: callback, context: context)
    Slot.session = session
    let ready = DispatchSemaphore(value: 0)
    var result: Int32 = -1
    Slot.task = Task {
        do {
            try await session.start(
                width: Int(width), height: Int(height), fps: Int(fps), bitrate: bitrate)
            result = 0
        } catch {
            NSLog("waystone: capture failed to start: \(error)")
            result = -2
        }
        ready.signal()
    }
    _ = ready.wait(timeout: .now() + 10)
    return result
}

/// Stops capture and releases the encoder.
@_cdecl("ws_capture_stop")
public func ws_capture_stop() {
    let session = Slot.session
    let task = Slot.task
    Slot.session = nil
    Slot.task = nil
    let ready = DispatchSemaphore(value: 0)
    Task {
        await session?.stop()
        _ = task
        ready.signal()
    }
    _ = ready.wait(timeout: .now() + 5)
}

/// Forces the next encoded frame to be an IDR.
@_cdecl("ws_capture_request_keyframe")
public func ws_capture_request_keyframe() {
    Slot.session?.requestKeyframe()
}

/// Updates the encoder bitrate, in bits per second.
@_cdecl("ws_capture_set_bitrate")
public func ws_capture_set_bitrate(_ bitrate: Int32) {
    Slot.session?.setBitrate(bitrate)
}

// MARK: - Input injection

import CoreGraphics

/// Injects one `waystone-proto` input frame into the local desktop.
///
/// The frame layout is documented in `crates/waystone-proto/src/input.rs`; it is
/// decoded here instead of in Rust because only CoreGraphics may post events.
@_cdecl("ws_input_handle")
public func ws_input_handle(_ data: UnsafePointer<UInt8>?, _ length: Int) -> Int32 {
    guard let data, length > 0 else { return -1 }
    let frame = UnsafeBufferPointer(start: data, count: length)
    switch frame[0] {
    case 0x01: // MouseMove
        guard length >= 5 else { return -1 }
        let x = Int(frame[1]) << 8 | Int(frame[2])
        let y = Int(frame[3]) << 8 | Int(frame[4])
        postMouseMove(normalizedX: x, normalizedY: y)
        return 0
    case 0x02: // MouseButton
        guard length >= 7 else { return -1 }
        let button = frame[1]
        let down = frame[2] != 0
        let x = Int(frame[3]) << 8 | Int(frame[4])
        let y = Int(frame[5]) << 8 | Int(frame[6])
        postMouseButton(button: button, down: down, normalizedX: x, normalizedY: y)
        return 0
    case 0x03: // MouseWheel
        guard length >= 5 else { return -1 }
        let dx = Int16(bitPattern: UInt16(frame[1]) << 8 | UInt16(frame[2]))
        let dy = Int16(bitPattern: UInt16(frame[3]) << 8 | UInt16(frame[4]))
        postWheel(dx: Int(dx), dy: Int(dy))
        return 0
    case 0x04: // Key
        guard length >= 7 else { return -1 }
        let code = Int(frame[1]) << 24 | Int(frame[2]) << 16 | Int(frame[3]) << 8 | Int(frame[4])
        let down = frame[5] != 0
        let modifiers = frame[6]
        postKey(code: code, down: down, modifiers: modifiers)
        return 0
    case 0x05: // Text
        let text = String(decoding: frame[1...], as: UTF8.self)
        postText(text)
        return 0
    case 0x06: // ReleaseAll
        releaseAll()
        return 0
    default:
        return -1
    }
}

/// Extent of the main display, used to map normalized coordinates.
private func displayBounds() -> CGRect {
    CGDisplayBounds(CGMainDisplayID())
}

/// Normalized `0...65535` to a global display point.
private func point(normalizedX x: Int, normalizedY y: Int) -> CGPoint {
    let bounds = displayBounds()
    return CGPoint(
        x: bounds.origin.x + bounds.width * CGFloat(x) / 65535.0,
        y: bounds.origin.y + bounds.height * CGFloat(y) / 65535.0
    )
}

private var lastPoint = CGPoint.zero
private var pressedButtons: Set<Int> = []
private var pressedKeys: Set<Int> = []

private func post(type: CGEventType, point: CGPoint, button: CGMouseButton = .left) {
    guard let event = CGEvent(mouseEventSource: nil, mouseType: type, mouseCursorPosition: point, mouseButton: button) else { return }
    event.post(tap: .cghidEventTap)
}

private func postMouseMove(normalizedX x: Int, normalizedY y: Int) {
    let target = point(normalizedX: x, normalizedY: y)
    lastPoint = target
    // Drag keeps the left button state; a plain move uses mouseMoved.
    if pressedButtons.contains(0) {
        post(type: .leftMouseDragged, point: target, button: .left)
    } else {
        post(type: .mouseMoved, point: target, button: .left)
    }
}

private func postMouseButton(button: UInt8, down: Bool, normalizedX x: Int, normalizedY y: Int) {
    let target = point(normalizedX: x, normalizedY: y)
    lastPoint = target
    let (cgButton, downType, upType, dragType): (CGMouseButton, CGEventType, CGEventType, CGEventType) = {
        switch button {
        case 1: return (.right, .rightMouseDown, .rightMouseUp, .rightMouseDragged)
        case 2: return (.center, .otherMouseDown, .otherMouseUp, .otherMouseDragged)
        default: return (.left, .leftMouseDown, .leftMouseUp, .leftMouseDragged)
        }
    }()
    let index = Int(button)
    if down {
        pressedButtons.insert(index)
        post(type: downType, point: target, button: cgButton)
    } else {
        pressedButtons.remove(index)
        post(type: upType, point: target, button: cgButton)
    }
    _ = dragType
}

private func postWheel(dx: Int, dy: Int) {
    guard let event = CGEvent(scrollWheelEvent2Source: nil, units: .pixel, wheelCount: 2, wheel1: Int32(dy), wheel2: Int32(dx), wheel3: 0) else { return }
    event.location = lastPoint
    event.post(tap: .cghidEventTap)
}

private func postKey(code: Int, down: Bool, modifiers: UInt8) {
    guard let event = CGEvent(keyboardEventSource: nil, virtualKey: CGKeyCode(code), keyDown: down) else { return }
    var flags: CGEventFlags = []
    if modifiers & 0x01 != 0 { flags.insert(.maskShift) }
    if modifiers & 0x02 != 0 { flags.insert(.maskControl) }
    if modifiers & 0x04 != 0 { flags.insert(.maskAlternate) }
    if modifiers & 0x08 != 0 { flags.insert(.maskCommand) }
    event.flags = flags
    event.post(tap: .cghidEventTap)
    if down { pressedKeys.insert(code) } else { pressedKeys.remove(code) }
}

private func postText(_ text: String) {
    for character in text {
        guard let event = CGEvent(keyboardEventSource: nil, virtualKey: 0, keyDown: true) else { continue }
        var unit = Array(String(character).utf16)
        event.keyboardSetUnicodeString(stringLength: unit.count, unicodeString: &unit)
        event.post(tap: .cghidEventTap)
    }
}

/// Releases every key and button the host thinks is held.
///
/// Sent when the session ends or the gesture preset changes, so a lost link never
/// leaves Ctrl stuck down on the desktop.
private func releaseAll() {
    for button in pressedButtons {
        let (cgButton, upType): (CGMouseButton, CGEventType) = {
            switch button {
            case 1: return (.right, .rightMouseUp)
            case 2: return (.center, .otherMouseUp)
            default: return (.left, .leftMouseUp)
            }
        }()
        post(type: upType, point: lastPoint, button: cgButton)
    }
    pressedButtons.removeAll()
    for code in pressedKeys {
        if let event = CGEvent(keyboardEventSource: nil, virtualKey: CGKeyCode(code), keyDown: false) {
            event.post(tap: .cghidEventTap)
        }
    }
    pressedKeys.removeAll()
}
