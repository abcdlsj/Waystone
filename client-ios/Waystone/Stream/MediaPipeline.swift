import CoreVideo
import Foundation

/// Transport → decoder → screen, plus audio, off the main thread.
///
/// Every frame stays on the transport and VideoToolbox threads; the main
/// thread only hears about things that change the UI (picture size) and is
/// polled for liveness by the session.
final class MediaPipeline: MediaSink {
    private let decoder: VideoDecoder
    private let audio: AudioPlayer
    private weak var view: VideoView?

    private let lock = NSLock()
    private var lastMedia = Date.distantPast
    private var lastPicture: Date?
    private var pictureSize: CGSize = .zero
    private var receivedFrames = 0

    /// Main-thread notification when the decoded size changes.
    var onPictureSize: ((CGSize) -> Void)?
    /// The decoder lost sync; the host should send a keyframe.
    var onKeyframeNeeded: (() -> Void)? {
        didSet { decoder.onKeyframeNeeded = onKeyframeNeeded }
    }

    init(codec: Wire.VideoCodec, audio: AudioPlayer) {
        decoder = VideoDecoder(codec: codec)
        self.audio = audio
        decoder.onFrame = { [weak self] buffer in self?.decoded(buffer) }
    }

    func attach(_ view: VideoView) {
        self.view = view
    }

    /// Time since the last media packet arrived, in seconds.
    var mediaAge: TimeInterval {
        lock.lock()
        defer { lock.unlock() }
        return Date().timeIntervalSince(lastMedia)
    }

    /// Time since the last decoded video frame was produced, in seconds.
    var pictureAge: TimeInterval {
        lock.lock()
        defer { lock.unlock() }
        guard let lastPicture else { return .infinity }
        return Date().timeIntervalSince(lastPicture)
    }

    var hasPicture: Bool {
        lock.lock()
        defer { lock.unlock() }
        return lastPicture != nil
    }

    var stats: (received: Int, decoder: VideoDecoder.Stats) {
        lock.lock()
        let received = receivedFrames
        lock.unlock()
        return (received, decoder.currentStats)
    }

    func shutdown() {
        decoder.invalidate()
        audio.stop()
    }

    // MARK: - MediaSink (transport thread)

    func video(_ frame: UnsafeBufferPointer<UInt8>, keyframe: Bool, rtpTime: UInt64) {
        lock.lock()
        lastMedia = Date()
        receivedFrames += 1
        lock.unlock()
        decoder.decode(Data(buffer: frame), rtpTime: rtpTime)
    }

    func audio(_ packet: UnsafeBufferPointer<UInt8>) {
        lock.lock()
        lastMedia = Date()
        lock.unlock()
        audio.enqueue(packet)
    }

    // MARK: - Decoder thread

    private func decoded(_ buffer: CVPixelBuffer) {
        let size = CGSize(width: CVPixelBufferGetWidth(buffer), height: CVPixelBufferGetHeight(buffer))
        lock.lock()
        lastPicture = Date()
        let changed = size != pictureSize
        pictureSize = size
        lock.unlock()
        if changed {
            DispatchQueue.main.async { [weak self] in self?.onPictureSize?(size) }
        }
        view?.present(buffer)
    }
}
