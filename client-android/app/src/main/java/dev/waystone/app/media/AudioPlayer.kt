package dev.waystone.app.media

import android.content.Context
import android.media.AudioAttributes
import android.media.AudioFormat
import android.media.AudioManager
import android.media.AudioTrack
import android.media.MediaCodec
import android.media.MediaFormat
import android.util.Log
import java.nio.ByteBuffer
import java.nio.ByteOrder
import java.util.concurrent.LinkedBlockingQueue
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicBoolean

/**
 * Low-latency 48 kHz stereo Opus playback using Android MediaCodec + AudioTrack.
 *
 * Follows design doc 5.1: in `auto` mode, respects the device's ringer silent/vibrate
 * state so opening a stream in a quiet room stays silent; `forceSound` always plays.
 */
class AudioPlayer(context: Context) {
    private val audioManager = context.getSystemService(Context.AUDIO_SERVICE) as AudioManager
    private val packets = LinkedBlockingQueue<ByteArray>(8)
    private val started = AtomicBoolean(false)

    @Volatile
    private var forceSound = false
    @Volatile
    private var muted = false

    private var codec: MediaCodec? = null
    private var track: AudioTrack? = null
    private var worker: Thread? = null

    fun start(forceSound: Boolean) {
        this.forceSound = forceSound
        if (!started.compareAndSet(false, true)) {
            updateVolume()
            return
        }
        try {
            val opusCodec = MediaCodec.createDecoderByType(MediaFormat.MIMETYPE_AUDIO_OPUS)
            val format = MediaFormat.createAudioFormat(
                MediaFormat.MIMETYPE_AUDIO_OPUS,
                SAMPLE_RATE,
                CHANNELS,
            ).apply {
                setByteBuffer("csd-0", buildOpusHead())
                setByteBuffer("csd-1", longLeBuffer(0L))
                setByteBuffer("csd-2", longLeBuffer(80_000_000L))
            }
            opusCodec.configure(format, null, null, 0)
            opusCodec.start()

            val minBuf = AudioTrack.getMinBufferSize(
                SAMPLE_RATE,
                AudioFormat.CHANNEL_OUT_STEREO,
                AudioFormat.ENCODING_PCM_16BIT,
            )
            val bufferSize = maxOf(minBuf, SAMPLE_RATE / 25 * CHANNELS * 2)
            val audioTrack = AudioTrack.Builder()
                .setAudioAttributes(
                    AudioAttributes.Builder()
                        .setUsage(AudioAttributes.USAGE_GAME)
                        .setContentType(AudioAttributes.CONTENT_TYPE_SONIFICATION)
                        .build()
                )
                .setAudioFormat(
                    AudioFormat.Builder()
                        .setEncoding(AudioFormat.ENCODING_PCM_16BIT)
                        .setSampleRate(SAMPLE_RATE)
                        .setChannelMask(AudioFormat.CHANNEL_OUT_STEREO)
                        .build()
                )
                .setBufferSizeInBytes(bufferSize)
                .setTransferMode(AudioTrack.MODE_STREAM)
                .setPerformanceMode(AudioTrack.PERFORMANCE_MODE_LOW_LATENCY)
                .build()

            codec = opusCodec
            track = audioTrack
            updateVolume()
            audioTrack.play()

            worker = Thread(::runLoop, "waystone-audio").apply {
                isDaemon = true
                start()
            }
        } catch (e: Exception) {
            Log.e(TAG, "cannot start audio player: ${e.message}")
            stop()
        }
    }

    fun stop() {
        started.set(false)
        packets.clear()
        worker?.interrupt()
        worker = null
        val t = track
        track = null
        if (t != null) {
            runCatching { t.pause() }
            runCatching { t.flush() }
            runCatching { t.release() }
        }
        val c = codec
        codec = null
        if (c != null) {
            runCatching { c.stop() }
            runCatching { c.release() }
        }
    }

    fun setForceSound(force: Boolean) {
        forceSound = force
        updateVolume()
    }

    fun setMuted(muted: Boolean) {
        this.muted = muted
        if (muted) packets.clear()
        updateVolume()
    }

    /** Enqueues one Opus packet from the transport thread. */
    fun enqueue(packet: ByteArray) {
        if (!started.get() || muted || packet.isEmpty()) return
        if (!packets.offer(packet)) {
            // Drop oldest packet so audio latency stays bounded.
            packets.poll()
            packets.offer(packet)
        }
    }

    private fun updateVolume() {
        val activeTrack = track ?: return
        val silent = muted || (!forceSound && audioManager.ringerMode != AudioManager.RINGER_MODE_NORMAL)
        runCatching { activeTrack.setVolume(if (silent) 0f else 1f) }
    }

    private fun runLoop() {
        val info = MediaCodec.BufferInfo()
        var ptsUs = 0L
        while (started.get()) {
            val packet = try {
                packets.poll(20, TimeUnit.MILLISECONDS)
            } catch (_: InterruptedException) {
                break
            } ?: continue
            val activeCodec = codec ?: break
            val activeTrack = track ?: break
            updateVolume()
            try {
                val inIdx = activeCodec.dequeueInputBuffer(5_000)
                if (inIdx >= 0) {
                    val buf = activeCodec.getInputBuffer(inIdx)
                    if (buf != null) {
                        buf.clear()
                        buf.put(packet)
                        activeCodec.queueInputBuffer(inIdx, 0, packet.size, ptsUs, 0)
                        ptsUs += 20_000L
                    }
                }
                while (started.get()) {
                    val outIdx = activeCodec.dequeueOutputBuffer(info, 0)
                    if (outIdx >= 0) {
                        val outBuf = activeCodec.getOutputBuffer(outIdx)
                        if (outBuf != null && info.size > 0 && !muted) {
                            outBuf.position(info.offset)
                            outBuf.limit(info.offset + info.size)
                            val pcm = ByteArray(info.size)
                            outBuf.get(pcm)
                            activeTrack.write(pcm, 0, pcm.size, AudioTrack.WRITE_NON_BLOCKING)
                        }
                        activeCodec.releaseOutputBuffer(outIdx, false)
                    } else {
                        break
                    }
                }
            } catch (e: Exception) {
                Log.w(TAG, "audio decode error: ${e.message}")
            }
        }
    }

    companion object {
        private const val TAG = "WaystoneAudio"
        const val SAMPLE_RATE = 48_000
        const val CHANNELS = 2

        /** 19-byte RFC 7845 OpusHead header for 48 kHz stereo. */
        private fun buildOpusHead(): ByteBuffer {
            val buf = ByteBuffer.allocate(19).order(ByteOrder.LITTLE_ENDIAN)
            buf.put("OpusHead".toByteArray(Charsets.US_ASCII))
            buf.put(1) // version
            buf.put(CHANNELS.toByte())
            buf.putShort(0) // pre-skip
            buf.putInt(SAMPLE_RATE)
            buf.putShort(0) // output gain
            buf.put(0) // channel mapping family
            buf.flip()
            return buf
        }

        private fun longLeBuffer(value: Long): ByteBuffer =
            ByteBuffer.allocate(8).order(ByteOrder.LITTLE_ENDIAN).putLong(value).apply { flip() }
    }
}
