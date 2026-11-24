// ScreenCaptureKit capture for one session, and its C entry points.
//
//   ScreenCaptureKit (IOSurface, NV12) -> VideoEncoder -> Annex-B -> callback
//   ScreenCaptureKit (48 kHz PCM)      -> OpusEncoder  -> 20 ms    -> callback
//
// A `CaptureSession` is created per stream and destroyed with it; there is no
// global state. All encoder work happens on `videoQueue`, so keyframe
// requests, re-encodes of the cached frame and new frames never race.

import CoreMedia
import CoreVideo
import Foundation
import IOKit.pwr_mgt
import ScreenCaptureKit

public typealias FrameCallback = @convention(c) (
    Int32, UnsafePointer<UInt8>?, Int, Bool, UInt64, UnsafeMutableRawPointer?
) -> Void
public typealias DoneCallback = @convention(c) (Int32, UnsafeMutableRawPointer?) -> Void

private let kindVideo: Int32 = 0
private let kindAudio: Int32 = 1

/// Thread safety is manual: `stateLock` guards `stream`/`stopped`, the rest is
/// confined to the two queues as marked.
final class CaptureSession: NSObject, SCStreamOutput, SCStreamDelegate, @unchecked Sendable {
    struct Settings {
        var display: CGDirectDisplayID
        var width: Int
        var height: Int
        var fps: Int
        var bitrate: Int32
        var codec: EncodeCodec
        var audio: Bool
    }

    /// Design doc 4.3: after 500 ms without a new frame the stream drops to a
    /// 5 fps keepalive, re-encoding the last frame. ScreenCaptureKit itself
    /// sends nothing while the screen is still, which would leave a viewer
    /// that lost a packet staring at a broken picture until the next change.
    static let idleAfter: TimeInterval = 0.5
    static let keepaliveInterval: TimeInterval = 0.2

    private var settings: Settings
    private let onFrame: FrameCallback
    private let context: UnsafeMutableRawPointer?

    private let videoQueue = DispatchQueue(label: "dev.waystone.capture.video", qos: .userInteractive)
    private let audioQueue = DispatchQueue(label: "dev.waystone.capture.audio", qos: .userInteractive)

    // Owned by `videoQueue`.
    private var encoder: VideoEncoder?
    private var lastFrame: CVPixelBuffer?
    private var lastFrameAt = Date.distantPast
    private var lastPresentation = CMTime.zero
    private var keepalive: DispatchSourceTimer?

    // Owned by `audioQueue`.
    private var opus: OpusEncoder?
    private var audioEnabled: Bool

    private var stream: SCStream?
    private let stateLock = NSLock()
    private var stopped = false
    private var awake = DisplayAwake()

    init(settings: Settings, onFrame: FrameCallback, context: UnsafeMutableRawPointer?) {
        self.settings = settings
        self.onFrame = onFrame
        self.context = context
        audioEnabled = settings.audio
    }

    /// Records the running stream; false when `stop` already ran.
    private func adopt(_ stream: SCStream) -> Bool {
        stateLock.lock()
        defer { stateLock.unlock() }
        self.stream = stream
        return !stopped
    }

    private var isStopped: Bool {
        stateLock.lock()
        defer { stateLock.unlock() }
        return stopped
    }

    // MARK: - Lifecycle

    func start() async throws {
        awake.hold()
        let display = try await Self.findDisplay(settings.display)
        let configuration = makeConfiguration()

        // The encoder is built for exactly what ScreenCaptureKit delivers;
        // a mismatch makes VideoToolbox rescale every frame.
        let encoder = try VideoEncoder(
            codec: settings.codec, width: settings.width, height: settings.height,
            fps: settings.fps, bitrate: settings.bitrate
        ) { [weak self] frame in self?.emit(kindVideo, frame.data, keyframe: frame.keyframe, micros: frame.micros) }
        videoQueue.sync { self.encoder = encoder }
        audioQueue.sync { self.opus = OpusEncoder() }

        let stream = SCStream(
            filter: SCContentFilter(display: display, excludingWindows: []),
            configuration: configuration, delegate: self)
        try stream.addStreamOutput(self, type: .screen, sampleHandlerQueue: videoQueue)
        try stream.addStreamOutput(self, type: .audio, sampleHandlerQueue: audioQueue)
        try await stream.startCapture()
        if adopt(stream) == false {
            // `stop` won the race while capture was starting.
            try? await stream.stopCapture()
            return
        }
        videoQueue.async { self.startKeepalive() }
    }

    /// Stops capture; no callback runs once this returns.
    func stop() {
        stateLock.lock()
        let already = stopped
        stopped = true
        let stream = self.stream
        self.stream = nil
        stateLock.unlock()
        guard !already else { return }

        if let stream {
            let done = DispatchSemaphore(value: 0)
            stream.stopCapture { _ in done.signal() }
            _ = done.wait(timeout: .now() + 3)
        }
        videoQueue.sync {
            keepalive?.cancel()
            keepalive = nil
            encoder?.finish()
            encoder = nil
            lastFrame = nil
        }
        audioQueue.sync { opus = nil }
        awake.release()
    }

    // MARK: - Controls

    func requestKeyframe() {
        videoQueue.async {
            guard let encoder = self.encoder else { return }
            if let frame = self.lastFrame {
                encoder.encode(frame, presentation: self.nextPresentation(), forceKeyframe: true)
            } else {
                encoder.forceNextKeyframe()
            }
        }
    }

    func setBitrate(_ bitrate: Int32) {
        videoQueue.async { self.encoder?.setBitrate(bitrate) }
    }

    func setFrameRate(_ fps: Int) {
        stateLock.lock()
        let stream = self.stream
        stateLock.unlock()
        guard fps != settings.fps, let stream else { return }
        settings.fps = fps
        stream.updateConfiguration(makeConfiguration()) { error in
            if let error { NSLog("waystone: frame rate change failed: \(error.localizedDescription)") }
        }
        videoQueue.async { self.encoder?.setFrameRate(fps) }
    }

    private func makeConfiguration() -> SCStreamConfiguration {
        let configuration = SCStreamConfiguration()
        configuration.width = settings.width
        configuration.height = settings.height
        configuration.minimumFrameInterval = CMTime(value: 1, timescale: CMTimeScale(settings.fps))
        configuration.pixelFormat = kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange
        configuration.colorSpaceName = CGColorSpace.sRGB
        configuration.showsCursor = true
        configuration.capturesAudio = true
        configuration.sampleRate = 48_000
        configuration.channelCount = 2
        configuration.excludesCurrentProcessAudio = true
        configuration.queueDepth = 4
        return configuration
    }

    func setAudio(_ enabled: Bool) {
        audioQueue.async { self.audioEnabled = enabled }
    }

    // MARK: - SCStreamOutput

    func stream(_ stream: SCStream, didOutputSampleBuffer sample: CMSampleBuffer, of type: SCStreamOutputType) {
        guard !isStopped else { return }
        switch type {
        case .screen:
            // Idle and blank frames carry no image; only complete ones count.
            guard Self.isComplete(sample), let pixels = sample.imageBuffer else { return }
            lastFrame = pixels
            lastFrameAt = Date()
            let presentation = CMSampleBufferGetPresentationTimeStamp(sample)
            lastPresentation = CMTimeMaximum(presentation, CMTimeAdd(lastPresentation, CMTime(value: 1, timescale: 1000)))
            encoder?.encode(pixels, presentation: lastPresentation)
        case .audio:
            guard audioEnabled else { return }
            opus?.encode(sample) { packet, micros in self.emit(kindAudio, packet, keyframe: false, micros: micros) }
        default:
            break
        }
    }

    func stream(_ stream: SCStream, didStopWithError error: Error) {
        NSLog("waystone: capture stopped: \(error.localizedDescription)")
    }

    // MARK: - Private

    private func emit(_ kind: Int32, _ data: [UInt8], keyframe: Bool, micros: UInt64) {
        guard !isStopped else { return }
        data.withUnsafeBufferPointer { onFrame(kind, $0.baseAddress, $0.count, keyframe, micros, context) }
    }

    private func startKeepalive() {
        let timer = DispatchSource.makeTimerSource(queue: videoQueue)
        timer.schedule(deadline: .now() + Self.keepaliveInterval, repeating: Self.keepaliveInterval)
        timer.setEventHandler { [weak self] in
            guard let self, let frame = self.lastFrame,
                Date().timeIntervalSince(self.lastFrameAt) >= Self.idleAfter
            else { return }
            self.encoder?.encode(frame, presentation: self.nextPresentation())
        }
        timer.resume()
        keepalive = timer
    }

    /// A presentation time after every one used so far; re-encoded frames
    /// need one and VideoToolbox rejects time going backwards.
    private func nextPresentation() -> CMTime {
        let now = CMClockGetTime(CMClockGetHostTimeClock())
        lastPresentation = CMTimeMaximum(now, CMTimeAdd(lastPresentation, CMTime(value: 1, timescale: 1000)))
        return lastPresentation
    }

    private static func isComplete(_ sample: CMSampleBuffer) -> Bool {
        guard let attachments = CMSampleBufferGetSampleAttachmentsArray(sample, createIfNecessary: false)
            as? [[SCStreamFrameInfo: Any]],
            let raw = attachments.first?[.status] as? Int,
            let status = SCFrameStatus(rawValue: raw)
        else { return true }
        return status == .complete
    }

    /// ScreenCaptureKit sometimes reports no displays right after launch or a
    /// wake, so the lookup retries briefly.
    private static func findDisplay(_ id: CGDirectDisplayID) async throws -> SCDisplay {
        for _ in 0..<5 {
            let content = try await SCShareableContent.excludingDesktopWindows(false, onScreenWindowsOnly: false)
            if let display = content.displays.first(where: { $0.displayID == id }) ?? content.displays.first {
                return display
            }
            try await Task.sleep(nanoseconds: 400_000_000)
        }
        throw CGDisplayIsAsleep(id) != 0 ? CaptureError.displayAsleep : CaptureError.noDisplay
    }
}

enum CaptureError: Error {
    case noDisplay
    case displayAsleep
    case encoderUnavailable(OSStatus)

    var status: Int32 {
        switch self {
        case .noDisplay: return -1
        case .displayAsleep: return -2
        case .encoderUnavailable: return -3
        }
    }
}

/// Keeps the display awake while streaming: ScreenCaptureKit reports no
/// display at all once the panel sleeps.
private struct DisplayAwake {
    private var assertion: IOPMAssertionID = 0

    mutating func hold() {
        guard assertion == 0 else { return }
        var wake: IOPMAssertionID = 0
        IOPMAssertionDeclareUserActivity("Waystone stream" as CFString, kIOPMUserActiveLocal, &wake)
        IOPMAssertionCreateWithName(
            kIOPMAssertionTypeNoDisplaySleep as CFString, IOPMAssertionLevel(kIOPMAssertionLevelOn),
            "Waystone is streaming this display" as CFString, &assertion)
    }

    mutating func release() {
        guard assertion != 0 else { return }
        IOPMAssertionRelease(assertion)
        assertion = 0
    }
}

// MARK: - C entry points

private func session(_ handle: UnsafeMutableRawPointer?) -> CaptureSession? {
    handle.map { Unmanaged<CaptureSession>.fromOpaque($0).takeUnretainedValue() }
}

@_cdecl("ws_capture_create")
public func ws_capture_create(
    _ display: UInt32, _ width: Int32, _ height: Int32, _ fps: Int32, _ bitrate: Int32,
    _ codec: Int32, _ audio: Bool, _ onFrame: FrameCallback, _ context: UnsafeMutableRawPointer?
) -> UnsafeMutableRawPointer {
    let settings = CaptureSession.Settings(
        display: display, width: Int(width), height: Int(height), fps: Int(fps), bitrate: bitrate,
        codec: codec == 1 ? .h264 : .hevc, audio: audio)
    let session = CaptureSession(settings: settings, onFrame: onFrame, context: context)
    return Unmanaged.passRetained(session).toOpaque()
}

/// Starts asynchronously and reports through `done` exactly once.
@_cdecl("ws_capture_start")
public func ws_capture_start(_ handle: UnsafeMutableRawPointer?, _ done: DoneCallback, _ doneContext: UnsafeMutableRawPointer?) {
    guard let capture = session(handle) else { return done(-4, doneContext) }
    Task.detached(priority: .userInitiated) {
        do {
            try await capture.start()
            done(0, doneContext)
        } catch let error as CaptureError {
            NSLog("waystone: capture failed: \(error)")
            done(error.status, doneContext)
        } catch {
            NSLog("waystone: capture failed: \(error.localizedDescription)")
            done(-4, doneContext)
        }
    }
}

/// Stops and releases the session; the handle is invalid afterwards.
@_cdecl("ws_capture_stop")
public func ws_capture_stop(_ handle: UnsafeMutableRawPointer?) {
    guard let handle else { return }
    let capture = Unmanaged<CaptureSession>.fromOpaque(handle)
    capture.takeUnretainedValue().stop()
    capture.release()
}

@_cdecl("ws_capture_request_keyframe")
public func ws_capture_request_keyframe(_ handle: UnsafeMutableRawPointer?) {
    session(handle)?.requestKeyframe()
}

@_cdecl("ws_capture_set_bitrate")
public func ws_capture_set_bitrate(_ handle: UnsafeMutableRawPointer?, _ bitrate: Int32) {
    session(handle)?.setBitrate(bitrate)
}

@_cdecl("ws_capture_set_fps")
public func ws_capture_set_fps(_ handle: UnsafeMutableRawPointer?, _ fps: Int32) {
    session(handle)?.setFrameRate(Int(fps))
}

@_cdecl("ws_capture_set_audio")
public func ws_capture_set_audio(_ handle: UnsafeMutableRawPointer?, _ enabled: Bool) {
    session(handle)?.setAudio(enabled)
}
