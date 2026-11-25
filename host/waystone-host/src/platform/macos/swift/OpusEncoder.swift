// System audio to 20 ms Opus packets with the AudioToolbox encoder.

import AudioToolbox
import CoreMedia
import Foundation

/// Encodes the 48 kHz stereo float audio ScreenCaptureKit delivers.
///
/// ScreenCaptureKit hands out buffers of arbitrary length, so samples are
/// accumulated and cut into 960-frame packets. Each packet's timestamp is the
/// time of its first sample: the first buffer anchors the clock and every
/// packet advances it by exactly 20 ms, re-anchoring only after a gap.
final class OpusEncoder {
    static let framesPerPacket = 960
    static let packetMicros: UInt64 = 20_000

    private var converter: AudioConverterRef?
    private var pending: [Float] = []
    private var pendingStart: UInt64 = 0
    private var output = [UInt8](repeating: 0, count: 1500)

    init() {
        var source = AudioStreamBasicDescription(
            mSampleRate: 48_000, mFormatID: kAudioFormatLinearPCM,
            mFormatFlags: kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked,
            mBytesPerPacket: 8, mFramesPerPacket: 1, mBytesPerFrame: 8, mChannelsPerFrame: 2,
            mBitsPerChannel: 32, mReserved: 0)
        var opus = AudioStreamBasicDescription(
            mSampleRate: 48_000, mFormatID: kAudioFormatOpus, mFormatFlags: 0, mBytesPerPacket: 0,
            mFramesPerPacket: UInt32(Self.framesPerPacket), mBytesPerFrame: 0, mChannelsPerFrame: 2,
            mBitsPerChannel: 0, mReserved: 0)
        let status = AudioConverterNew(&source, &opus, &converter)
        guard status == noErr, let converter else {
            NSLog("waystone: no Opus encoder (\(status))")
            return
        }
        var bitrate: UInt32 = 96_000
        AudioConverterSetProperty(converter, kAudioConverterEncodeBitRate, 4, &bitrate)
    }

    deinit {
        if let converter { AudioConverterDispose(converter) }
    }

    func encode(_ sample: CMSampleBuffer, emit: ([UInt8], UInt64) -> Void) {
        guard converter != nil,
            let format = CMSampleBufferGetFormatDescription(sample),
            let description = CMAudioFormatDescriptionGetStreamBasicDescription(format)?.pointee,
            description.mFormatFlags & kAudioFormatFlagIsFloat != 0,
            description.mChannelsPerFrame == 2
        else { return }

        let start = UInt64(max(0, CMSampleBufferGetPresentationTimeStamp(sample).seconds) * 1_000_000)
        let expected = pendingStart + UInt64(pending.count / 2) * 1_000_000 / 48_000
        if pending.isEmpty || start > expected + 2 * Self.packetMicros {
            pending.removeAll(keepingCapacity: true)
            pendingStart = start
        }
        append(sample, nonInterleaved: description.mFormatFlags & kAudioFormatFlagIsNonInterleaved != 0)

        let packetSamples = Self.framesPerPacket * 2
        while pending.count >= packetSamples {
            if let packet = encodePacket(Array(pending[0..<packetSamples])) {
                emit(packet, pendingStart)
            }
            pending.removeFirst(packetSamples)
            pendingStart += Self.packetMicros
        }
    }

    /// Appends the buffer as interleaved stereo.
    private func append(_ sample: CMSampleBuffer, nonInterleaved: Bool) {
        var size = 0
        CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer(
            sample, bufferListSizeNeededOut: &size, bufferListOut: nil, bufferListSize: 0,
            blockBufferAllocator: nil, blockBufferMemoryAllocator: nil, flags: 0, blockBufferOut: nil)
        let raw = UnsafeMutableRawPointer.allocate(byteCount: size, alignment: 16)
        defer { raw.deallocate() }
        let list = raw.bindMemory(to: AudioBufferList.self, capacity: 1)
        var block: CMBlockBuffer?
        guard CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer(
            sample, bufferListSizeNeededOut: nil, bufferListOut: list, bufferListSize: size,
            blockBufferAllocator: nil, blockBufferMemoryAllocator: nil,
            flags: kCMSampleBufferFlag_AudioBufferList_Assure16ByteAlignment, blockBufferOut: &block) == noErr
        else { return }
        let buffers = UnsafeMutableAudioBufferListPointer(list)
        if nonInterleaved, buffers.count >= 2,
            let left = buffers[0].mData?.assumingMemoryBound(to: Float.self),
            let right = buffers[1].mData?.assumingMemoryBound(to: Float.self)
        {
            let frames = Int(buffers[0].mDataByteSize) / 4
            pending.reserveCapacity(pending.count + frames * 2)
            for index in 0..<frames {
                pending.append(left[index])
                pending.append(right[index])
            }
        } else if let first = buffers.first, let data = first.mData?.assumingMemoryBound(to: Float.self) {
            pending.append(contentsOf: UnsafeBufferPointer(start: data, count: Int(first.mDataByteSize) / 4))
        }
    }

    private func encodePacket(_ samples: [Float]) -> [UInt8]? {
        guard let converter else { return nil }
        return samples.withUnsafeBufferPointer { input -> [UInt8]? in
            var source = PcmSource(
                data: UnsafeMutableRawPointer(mutating: input.baseAddress!), bytes: UInt32(input.count * 4))
            var packets: UInt32 = 1
            var description = AudioStreamPacketDescription()
            let status: OSStatus = output.withUnsafeMutableBytes { out in
                var list = AudioBufferList(
                    mNumberBuffers: 1,
                    mBuffers: AudioBuffer(mNumberChannels: 2, mDataByteSize: UInt32(out.count), mData: out.baseAddress))
                return AudioConverterFillComplexBuffer(
                    converter, PcmSource.supply, &source, &packets, &list, &description)
            }
            guard status == noErr || source.consumed, packets == 1, description.mDataByteSize > 0 else {
                if status != noErr { NSLog("waystone: opus encode failed (\(status))") }
                return nil
            }
            let start = Int(description.mStartOffset)
            return Array(output[start..<start + Int(description.mDataByteSize)])
        }
    }

    /// Feeds one packet worth of PCM to the converter, then reports the end.
    private struct PcmSource {
        var data: UnsafeMutableRawPointer
        var bytes: UInt32
        var consumed = false

        static let supply: AudioConverterComplexInputDataProc = { _, packets, buffers, _, user in
            let source = user!.assumingMemoryBound(to: PcmSource.self)
            guard !source.pointee.consumed else {
                packets.pointee = 0
                return 1  // "no more data for now", not an error
            }
            source.pointee.consumed = true
            packets.pointee = source.pointee.bytes / 8
            buffers.pointee.mNumberBuffers = 1
            buffers.pointee.mBuffers.mData = source.pointee.data
            buffers.pointee.mBuffers.mDataByteSize = source.pointee.bytes
            buffers.pointee.mBuffers.mNumberChannels = 2
            return noErr
        }
    }
}
