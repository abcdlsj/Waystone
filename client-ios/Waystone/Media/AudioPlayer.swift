import AVFoundation
import AudioToolbox
import Foundation

/// Opus playback with a small jitter buffer.
///
/// Packets are decoded with the system Opus decoder as they arrive and the PCM
/// goes into a ring buffer that the audio engine drains. The buffer is capped
/// at `maxBuffered` so latency cannot creep up; when it overflows, the oldest
/// audio is dropped.
///
/// The session category follows design doc 5.1: by default `.ambient`, which
/// obeys the ring/silent switch, so opening a stream in a quiet office stays
/// silent. Turning sound on explicitly switches to `.playback`.
final class AudioPlayer {
    private let engine = AVAudioEngine()
    private let decoder: OpusDecoder?
    private let ring = SampleRing(capacity: 48_000 / 10 * 2)  // 100 ms stereo
    private var started = false

    static let sampleRate = 48_000.0

    init() {
        decoder = OpusDecoder()
    }

    deinit { stop() }

    /// `forceSound` plays even when the phone is on silent.
    func start(forceSound: Bool) {
        guard !started, decoder != nil else { return }
        let session = AVAudioSession.sharedInstance()
        do {
            try session.setCategory(forceSound ? .playback : .ambient, mode: .default, options: [.mixWithOthers])
            try session.setPreferredSampleRate(Self.sampleRate)
            try session.setPreferredIOBufferDuration(0.01)
            try session.setActive(true)
        } catch {
            Log.media.error("audio session: \(error.localizedDescription)")
        }

        let format = AVAudioFormat(standardFormatWithSampleRate: Self.sampleRate, channels: 2)!
        let ring = self.ring
        let node = AVAudioSourceNode(format: format) { _, _, frameCount, bufferList in
            let buffers = UnsafeMutableAudioBufferListPointer(bufferList)
            guard buffers.count >= 2,
                let left = buffers[0].mData?.assumingMemoryBound(to: Float.self),
                let right = buffers[1].mData?.assumingMemoryBound(to: Float.self)
            else { return noErr }
            ring.readDeinterleaved(left: left, right: right, frames: Int(frameCount))
            return noErr
        }
        engine.attach(node)
        engine.connect(node, to: engine.mainMixerNode, format: format)
        do {
            try engine.start()
            started = true
        } catch {
            Log.media.error("audio engine: \(error.localizedDescription)")
        }
    }

    func stop() {
        guard started else { return }
        started = false
        engine.stop()
        ring.clear()
        try? AVAudioSession.sharedInstance().setActive(false, options: .notifyOthersOnDeactivation)
    }

    /// Switches between "follow the silent switch" and "always play".
    func setForceSound(_ force: Bool) {
        try? AVAudioSession.sharedInstance().setCategory(
            force ? .playback : .ambient, mode: .default, options: [.mixWithOthers])
    }

    func setMuted(_ muted: Bool) {
        engine.mainMixerNode.outputVolume = muted ? 0 : 1
        if muted { ring.clear() }
    }

    /// Decodes one packet; called on the transport thread.
    func enqueue(_ packet: UnsafeBufferPointer<UInt8>) {
        guard started, let decoder else { return }
        decoder.decode(packet) { ring.write(interleaved: $0, frames: $1) }
    }
}

/// The system Opus decoder behind `AudioConverter`.
private final class OpusDecoder {
    private var converter: AudioConverterRef?
    private var output = [Float](repeating: 0, count: 5760 * 2)

    init?() {
        var input = AudioStreamBasicDescription(
            mSampleRate: AudioPlayer.sampleRate, mFormatID: kAudioFormatOpus, mFormatFlags: 0,
            mBytesPerPacket: 0, mFramesPerPacket: 960, mBytesPerFrame: 0, mChannelsPerFrame: 2,
            mBitsPerChannel: 0, mReserved: 0)
        var pcm = AudioStreamBasicDescription(
            mSampleRate: AudioPlayer.sampleRate, mFormatID: kAudioFormatLinearPCM,
            mFormatFlags: kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked,
            mBytesPerPacket: 8, mFramesPerPacket: 1, mBytesPerFrame: 8, mChannelsPerFrame: 2,
            mBitsPerChannel: 32, mReserved: 0)
        guard AudioConverterNew(&input, &pcm, &converter) == noErr else { return nil }
    }

    deinit {
        if let converter { AudioConverterDispose(converter) }
    }

    /// Calls `sink` with interleaved stereo floats and the frame count.
    func decode(_ packet: UnsafeBufferPointer<UInt8>, sink: (UnsafePointer<Float>, Int) -> Void) {
        guard let converter, let base = packet.baseAddress, !packet.isEmpty else { return }
        var source = PacketSource(data: UnsafeMutableRawPointer(mutating: base), size: UInt32(packet.count))
        var frames = UInt32(output.count / 2)
        let status: OSStatus = output.withUnsafeMutableBufferPointer { pcm in
            var list = AudioBufferList(
                mNumberBuffers: 1,
                mBuffers: AudioBuffer(
                    mNumberChannels: 2, mDataByteSize: UInt32(pcm.count * 4), mData: pcm.baseAddress))
            return AudioConverterFillComplexBuffer(converter, PacketSource.supply, &source, &frames, &list, nil)
        }
        guard status == noErr || source.consumed, frames > 0 else { return }
        output.withUnsafeBufferPointer { sink($0.baseAddress!, Int(frames)) }
    }

    /// Hands the converter exactly one packet, with the description that
    /// variable-bitrate input requires.
    private struct PacketSource {
        var data: UnsafeMutableRawPointer
        var size: UInt32
        var consumed = false
        var description = AudioStreamPacketDescription()

        static let supply: AudioConverterComplexInputDataProc = { _, packets, buffers, descriptions, user in
            let source = user!.assumingMemoryBound(to: PacketSource.self)
            guard !source.pointee.consumed else {
                packets.pointee = 0
                // Any non-zero status ends this fill call without an error.
                return 1
            }
            source.pointee.consumed = true
            packets.pointee = 1
            buffers.pointee.mNumberBuffers = 1
            buffers.pointee.mBuffers.mData = source.pointee.data
            buffers.pointee.mBuffers.mDataByteSize = source.pointee.size
            buffers.pointee.mBuffers.mNumberChannels = 2
            source.pointee.description = AudioStreamPacketDescription(
                mStartOffset: 0, mVariableFramesInPacket: 0, mDataByteSize: source.pointee.size)
            let offset = MemoryLayout<PacketSource>.offset(of: \PacketSource.description)!
            descriptions?.pointee = UnsafeMutableRawPointer(source).advanced(by: offset)
                .assumingMemoryBound(to: AudioStreamPacketDescription.self)
            return noErr
        }
    }
}

/// Interleaved stereo ring buffer shared by the decoder thread and the
/// real-time audio thread. The lock is held for a few hundred copies at most.
private final class SampleRing {
    private var storage: [Float]
    private var read = 0
    private var count = 0
    private let lock = NSLock()

    init(capacity: Int) {
        storage = [Float](repeating: 0, count: capacity)
    }

    func write(interleaved samples: UnsafePointer<Float>, frames: Int) {
        lock.lock()
        defer { lock.unlock() }
        let capacity = storage.count
        for index in 0..<(frames * 2) {
            storage[(read + count) % capacity] = samples[index]
            if count < capacity {
                count += 1
            } else {
                read = (read + 1) % capacity
            }
        }
    }

    func readDeinterleaved(left: UnsafeMutablePointer<Float>, right: UnsafeMutablePointer<Float>, frames: Int) {
        lock.lock()
        defer { lock.unlock() }
        let capacity = storage.count
        for frame in 0..<frames {
            if count >= 2 {
                left[frame] = storage[read]
                right[frame] = storage[(read + 1) % capacity]
                read = (read + 2) % capacity
                count -= 2
            } else {
                left[frame] = 0
                right[frame] = 0
            }
        }
    }

    func clear() {
        lock.lock()
        read = 0
        count = 0
        lock.unlock()
    }
}
