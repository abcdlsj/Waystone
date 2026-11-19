import AVFoundation
import AudioToolbox
import Foundation

/// Low latency Opus playback through `AVAudioEngine`.
///
/// The decoded audio is fed as PCM into an `AVAudioSourceNode` so the phone can
/// keep its own jitter buffer at 20–30 ms, instead of the 150 ms default of
/// `AVAudioPlayer`.
final class AudioPlayer {
    private let engine = AVAudioEngine()
    private var converter: AudioConverterRef?
    private var sourceNode: AVAudioSourceNode?
    private var ring = RingBuffer(capacity: 48_000)
    private let lock = NSLock()

    /// True while audio is muted because the phone is on silent mode.
    private(set) var isMuted = false

    init() {}

    deinit {
        engine.stop()
        if let converter {
            AudioConverterDispose(converter)
        }
    }

    /// Starts the audio session and the engine.
    func start() throws {
        let session = AVAudioSession.sharedInstance()
        // `.playback` keeps the stream going when the ringer switch is silent,
        // which is why the mute decision is taken explicitly by the UI instead.
        try session.setCategory(.playback, mode: .moviePlayback, options: [.mixWithOthers])
        try session.setPreferredSampleRate(48_000)
        try session.setPreferredIOBufferDuration(0.02)
        try session.setActive(true)

        isMuted = session.outputVolume == 0

        guard let converter = makeConverter() else {
            throw NSError(domain: "waystone.audio", code: -1)
        }
        self.converter = converter

        let format = AVAudioFormat(commonFormat: .pcmFormatFloat32, sampleRate: 48_000, channels: 2, interleaved: false)!
        let node = AVAudioSourceNode(format: format) { [weak self] _, _, frameCount, audioBufferList in
            guard let self else { return noErr }
            let buffers = UnsafeMutableAudioBufferListPointer(audioBufferList)
            let frames = Int(frameCount)
            let samples = self.ring.read(count: frames * 2)
            for channel in 0..<buffers.count {
                guard let data = buffers[channel].mData?.assumingMemoryBound(to: Float.self) else { continue }
                for frame in 0..<frames {
                    let index = frame * 2 + min(channel, 1)
                    data[frame] = index < samples.count ? samples[index] : 0
                }
            }
            return noErr
        }
        sourceNode = node
        engine.attach(node)
        engine.connect(node, to: engine.mainMixerNode, format: format)
        engine.prepare()
        try engine.start()
    }

    func stop() {
        engine.stop()
        if let node = sourceNode {
            engine.detach(node)
            sourceNode = nil
        }
        try? AVAudioSession.sharedInstance().setActive(false, options: .notifyOthersOnDeactivation)
    }

    /// Enqueues one Opus packet for decoding and playback.
    func enqueue(opus: [UInt8]) {
        guard !opus.isEmpty else { return }
        lock.lock()
        let converter = self.converter
        lock.unlock()
        guard let converter else { return }

        guard let pcm = decode(converter: converter, opus: opus) else { return }
        lock.lock()
        ring.write(pcm)
        lock.unlock()
    }

    /// Drops everything buffered, for example after a stall.
    func flush() {
        lock.lock()
        ring.clear()
        lock.unlock()
    }

    /// Mutes or unmutes without tearing the engine down.
    func setMuted(_ muted: Bool) {
        isMuted = muted
        engine.mainMixerNode.outputVolume = muted ? 0 : 1
    }

    private func makeConverter() -> AudioConverterRef? {
        var source = AudioStreamBasicDescription(
            mSampleRate: 48_000,
            mFormatID: kAudioFormatOpus,
            mFormatFlags: 0,
            mBytesPerPacket: 0,
            mFramesPerPacket: 960,
            mBytesPerFrame: 0,
            mChannelsPerFrame: 2,
            mBitsPerChannel: 0,
            mReserved: 0)
        var destination = AudioStreamBasicDescription(
            mSampleRate: 48_000,
            mFormatID: kAudioFormatLinearPCM,
            mFormatFlags: kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked,
            mBytesPerPacket: 8,
            mFramesPerPacket: 1,
            mBytesPerFrame: 8,
            mChannelsPerFrame: 2,
            mBitsPerChannel: 32,
            mReserved: 0)
        var converter: AudioConverterRef?
        guard AudioConverterNew(&source, &destination, &converter) == noErr else { return nil }
        // Opus needs the identification header as a magic cookie.
        var header = Self.opusHead()
        _ = header.withUnsafeMutableBytes { raw -> OSStatus in
            guard let base = raw.baseAddress else { return -1 }
            return AudioConverterSetProperty(
                converter!, kAudioConverterCompressionMagicCookie, UInt32(raw.count), base)
        }
        return converter
    }

    private func decode(converter: AudioConverterRef, opus: [UInt8]) -> [Float]? {
        final class Box {
            let packet: [UInt8]
            var consumed = false
            init(_ packet: [UInt8]) { self.packet = packet }
        }
        let box = Box(opus)
        let context = Unmanaged.passUnretained(box).toOpaque()

        var pcm = [Float](repeating: 0, count: 5760 * 2)
        var frames: UInt32 = 5760
        let bufferList = AudioBufferList.allocate(maximumBuffers: 1)
        defer { free(bufferList.unsafeMutablePointer) }

        let status: OSStatus = pcm.withUnsafeMutableBytes { raw -> OSStatus in
            bufferList[0] = AudioBuffer(
                mNumberChannels: 2,
                mDataByteSize: UInt32(raw.count),
                mData: raw.baseAddress)
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
                    ioPackets.pointee = 1
                    let destination = UnsafeMutableAudioBufferListPointer(ioData)
                    box.packet.withUnsafeBytes { raw in
                        destination[0].mData = UnsafeMutableRawPointer(mutating: raw.baseAddress)
                        destination[0].mDataByteSize = UInt32(box.packet.count)
                        destination[0].mNumberChannels = 2
                    }
                    return noErr
                },
                context,
                &frames,
                bufferList.unsafeMutablePointer,
                nil)
        }
        guard status == noErr, frames > 0 else { return nil }
        return Array(pcm.prefix(Int(frames) * 2))
    }

    /// Minimal OpusHead for 48 kHz stereo.
    private static func opusHead() -> [UInt8] {
        var head: [UInt8] = Array("OpusHead".utf8)
        head.append(1)  // version
        head.append(2)  // channels
        head.append(contentsOf: [0x38, 0x01])  // pre-skip, 312 samples
        head.append(contentsOf: [0x80, 0xBB, 0x00, 0x00])  // input sample rate, 48000
        head.append(contentsOf: [0x00, 0x00])  // output gain
        head.append(0)  // channel mapping family
        return head
    }
}

/// Lock protected float ring buffer.
final class RingBuffer {
    private var storage: [Float]
    private var readIndex = 0
    private var writeIndex = 0
    private var count = 0
    private let capacity: Int

    init(capacity: Int) {
        self.capacity = capacity
        self.storage = [Float](repeating: 0, count: capacity)
    }

    func write(_ samples: [Float]) {
        for sample in samples {
            storage[writeIndex] = sample
            writeIndex = (writeIndex + 1) % capacity
            if count < capacity {
                count += 1
            } else {
                // Overflow: drop the oldest sample so the latency stays bounded.
                readIndex = (readIndex + 1) % capacity
            }
        }
    }

    /// Reads up to `count` samples, zero padding when the buffer runs dry.
    func read(count wanted: Int) -> [Float] {
        var output = [Float](repeating: 0, count: wanted)
        let available = min(wanted, count)
        for index in 0..<available {
            output[index] = storage[(readIndex + index) % capacity]
        }
        readIndex = (readIndex + available) % capacity
        count -= available
        return output
    }

    func clear() {
        readIndex = 0
        writeIndex = 0
        count = 0
    }
}
