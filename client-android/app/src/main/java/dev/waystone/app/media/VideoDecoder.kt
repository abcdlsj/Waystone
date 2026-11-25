package dev.waystone.app.media

import android.media.MediaCodec
import android.media.MediaFormat
import android.os.Build
import android.util.Log
import android.view.Surface
import dev.waystone.app.protocol.Wire
import java.io.ByteArrayOutputStream
import java.nio.ByteBuffer
import java.util.concurrent.LinkedBlockingQueue
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicInteger

/** NAL unit rules that differ between H.264 and HEVC. */
enum class NalSyntax {
    H264,
    HEVC;

    fun nalType(headerByte: Byte): Int {
        val v = headerByte.toInt() and 0xff
        return when (this) {
            HEVC -> (v ushr 1) and 0x3f
            H264 -> v and 0x1f
        }
    }

    fun isParameterSet(type: Int): Boolean = when (this) {
        HEVC -> type in 32..34
        H264 -> type == 7 || type == 8
    }

    fun isKeyframe(type: Int): Boolean = when (this) {
        HEVC -> type in 16..21
        H264 -> type == 5
    }

    companion object {
        fun from(codec: Wire.VideoCodec): NalSyntax =
            if (codec == Wire.VideoCodec.H264) H264 else HEVC
    }
}

/**
 * Hardware MediaCodec decoder for Annex-B H.264 / HEVC rendering directly to a Surface.
 *
 * Mirrors iOS `VideoDecoder`: rebuilds when parameter sets change, bounds in-flight
 * frames so backlog never turns into latency, and requests a fresh keyframe on loss.
 */
class VideoDecoder(
    private val videoCodec: Wire.VideoCodec,
) {
    data class Stats(
        val decoded: Int = 0,
        val skipped: Int = 0,
        val failed: Int = 0,
    )

    private data class QueuedFrame(
        val data: ByteArray,
        val rtpTime: Long,
    )

    var onFrameRendered: (() -> Unit)? = null
    var onPictureSize: ((width: Int, height: Int) -> Unit)? = null
    var onKeyframeNeeded: (() -> Unit)? = null

    private val syntax = NalSyntax.from(videoCodec)
    private val mimeType = if (videoCodec == Wire.VideoCodec.H264) {
        MediaFormat.MIMETYPE_VIDEO_AVC
    } else {
        MediaFormat.MIMETYPE_VIDEO_HEVC
    }

    private val queue = LinkedBlockingQueue<QueuedFrame>(16)
    private val running = AtomicBoolean(true)
    private val inFlight = AtomicInteger(0)

    private val statsLock = Any()
    private var decodedCount = 0
    private var skippedCount = 0
    private var failedCount = 0

    private val codecLock = Any()
    @Volatile
    private var surface: Surface? = null
    private var codec: MediaCodec? = null
    private var drainThread: Thread? = null
    private var parameterSets: List<ByteArray> = emptyList()
    private var needsKeyframe = true
    private var currentWidth = 0
    private var currentHeight = 0

    private val decodeThread = Thread(::decodeLoop, "waystone-video-decode").apply {
        isDaemon = true
        start()
    }

    val currentStats: Stats
        get() = synchronized(statsLock) { Stats(decodedCount, skippedCount, failedCount) }

    fun attachSurface(newSurface: Surface?) {
        val changed = synchronized(codecLock) {
            if (surface === newSurface) return
            surface = newSurface
            releaseCodecLocked()
            needsKeyframe = true
            newSurface != null
        }
        if (changed) {
            onKeyframeNeeded?.invoke()
        }
    }

    /** Queues one Annex-B access unit from the transport thread. */
    fun decode(accessUnit: ByteArray, rtpTime: Long) {
        if (!running.get()) return
        if (!queue.offer(QueuedFrame(accessUnit, rtpTime))) {
            // Queue overflow: drop stale frames and ask for an IDR.
            queue.clear()
            synchronized(statsLock) { skippedCount += 1 }
            markKeyframeNeeded()
            queue.offer(QueuedFrame(accessUnit, rtpTime))
        }
    }

    fun release() {
        if (!running.compareAndSet(true, false)) return
        queue.clear()
        decodeThread.interrupt()
        synchronized(codecLock) {
            surface = null
            releaseCodecLocked()
        }
        onFrameRendered = null
        onPictureSize = null
        onKeyframeNeeded = null
    }

    private fun decodeLoop() {
        while (running.get()) {
            val frame = try {
                queue.poll(50, TimeUnit.MILLISECONDS)
            } catch (_: InterruptedException) {
                break
            } ?: continue
            processFrame(frame)
        }
    }

    private fun processFrame(frame: QueuedFrame) {
        val units = splitAnnexB(frame.data)
        if (units.isEmpty()) return

        val sets = mutableListOf<ByteArray>()
        var hasSlice = false
        var isKeyframe = false
        for (nal in units) {
            val type = syntax.nalType(nal[0])
            if (syntax.isParameterSet(type)) {
                sets.add(nal)
            } else {
                hasSlice = true
                if (syntax.isKeyframe(type)) isKeyframe = true
            }
        }
        if (!hasSlice) return

        val activeCodec: MediaCodec = synchronized(codecLock) {
            val activeSurface = surface ?: return
            if (sets.isNotEmpty() && (!sameParameterSets(sets, parameterSets) || codec == null)) {
                if (!rebuildCodecLocked(sets, activeSurface)) {
                    onKeyframeNeeded?.invoke()
                    return
                }
            }
            val c = codec ?: return
            if (needsKeyframe) {
                if (!isKeyframe) return
                needsKeyframe = false
            }
            if (!isKeyframe && inFlight.get() >= MAX_IN_FLIGHT) {
                synchronized(statsLock) { skippedCount += 1 }
                needsKeyframe = true
                onKeyframeNeeded?.invoke()
                return
            }
            c
        }

        val payload = if (isKeyframe && sets.isNotEmpty()) {
            prependStartCodes(sets + units.filter { !syntax.isParameterSet(syntax.nalType(it[0])) })
        } else {
            frame.data
        }

        try {
            val inputIndex = activeCodec.dequeueInputBuffer(10_000)
            if (inputIndex < 0) {
                if (!isKeyframe) {
                    synchronized(statsLock) { skippedCount += 1 }
                    markKeyframeNeeded()
                }
                return
            }
            val inputBuffer = activeCodec.getInputBuffer(inputIndex) ?: return
            inputBuffer.clear()
            if (inputBuffer.capacity() < payload.size) {
                activeCodec.queueInputBuffer(inputIndex, 0, 0, 0, 0)
                synchronized(statsLock) { failedCount += 1 }
                markKeyframeNeeded()
                return
            }
            inputBuffer.put(payload)
            val ptsUs = (frame.rtpTime * 1_000_000L) / 90_000L
            val flags = if (isKeyframe) MediaCodec.BUFFER_FLAG_KEY_FRAME else 0
            inFlight.incrementAndGet()
            activeCodec.queueInputBuffer(inputIndex, 0, payload.size, ptsUs, flags)
        } catch (e: Exception) {
            Log.e(TAG, "video decode queue failed: ${e.message}")
            inFlight.set(0)
            synchronized(statsLock) { failedCount += 1 }
            synchronized(codecLock) { releaseCodecLocked() }
            markKeyframeNeeded()
        }
    }

    private fun rebuildCodecLocked(sets: List<ByteArray>, targetSurface: Surface): Boolean {
        releaseCodecLocked()
        val created = try {
            MediaCodec.createDecoderByType(mimeType)
        } catch (e: Exception) {
            Log.e(TAG, "cannot create decoder for $mimeType: ${e.message}")
            return false
        }

        val format = MediaFormat.createVideoFormat(mimeType, 1920, 1080)
        format.setInteger(MediaFormat.KEY_PRIORITY, 0)
        attachCsd(format, sets)

        val configured = tryConfigure(created, format, targetSurface, lowLatency = true) ||
            tryConfigure(created, format, targetSurface, lowLatency = false)
        if (!configured) {
            runCatching { created.release() }
            return false
        }

        try {
            created.start()
        } catch (e: Exception) {
            Log.e(TAG, "cannot start decoder: ${e.message}")
            runCatching { created.release() }
            return false
        }

        codec = created
        parameterSets = sets.map { it.copyOf() }
        needsKeyframe = true
        inFlight.set(0)
        drainThread = Thread({ drainLoop(created) }, "waystone-video-drain").apply {
            isDaemon = true
            start()
        }
        return true
    }

    private fun tryConfigure(
        decoder: MediaCodec,
        baseFormat: MediaFormat,
        targetSurface: Surface,
        lowLatency: Boolean,
    ): Boolean {
        return try {
            if (lowLatency && Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
                baseFormat.setInteger(MediaFormat.KEY_LOW_LATENCY, 1)
            } else if (!lowLatency && Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
                baseFormat.setInteger(MediaFormat.KEY_LOW_LATENCY, 0)
            }
            decoder.configure(baseFormat, targetSurface, null, 0)
            true
        } catch (e: Exception) {
            Log.w(TAG, "configure(lowLatency=$lowLatency) failed: ${e.message}")
            false
        }
    }

    private fun attachCsd(format: MediaFormat, sets: List<ByteArray>) {
        if (syntax == NalSyntax.H264) {
            val sps = sets.firstOrNull { syntax.nalType(it[0]) == 7 }
            val pps = sets.firstOrNull { syntax.nalType(it[0]) == 8 }
            if (sps != null) format.setByteBuffer("csd-0", ByteBuffer.wrap(withStartCode(sps)))
            if (pps != null) format.setByteBuffer("csd-1", ByteBuffer.wrap(withStartCode(pps)))
        } else {
            format.setByteBuffer("csd-0", ByteBuffer.wrap(prependStartCodes(sets)))
        }
    }

    private fun drainLoop(activeCodec: MediaCodec) {
        val info = MediaCodec.BufferInfo()
        while (running.get() && codec === activeCodec) {
            val index = try {
                activeCodec.dequeueOutputBuffer(info, 10_000)
            } catch (_: Exception) {
                break
            }
            when {
                index >= 0 -> {
                    inFlight.updateAndGet { maxOf(0, it - 1) }
                    try {
                        activeCodec.releaseOutputBuffer(index, true)
                        synchronized(statsLock) { decodedCount += 1 }
                        onFrameRendered?.invoke()
                    } catch (_: Exception) {
                        break
                    }
                }
                index == MediaCodec.INFO_OUTPUT_FORMAT_CHANGED -> {
                    runCatching {
                        val fmt = activeCodec.outputFormat
                        val (w, h) = extractPictureSize(fmt)
                        if (w > 0 && h > 0 && (w != currentWidth || h != currentHeight)) {
                            currentWidth = w
                            currentHeight = h
                            onPictureSize?.invoke(w, h)
                        }
                    }
                }
            }
        }
    }

    private fun releaseCodecLocked() {
        val old = codec
        codec = null
        drainThread?.interrupt()
        drainThread = null
        inFlight.set(0)
        if (old != null) {
            runCatching { old.stop() }
            runCatching { old.release() }
        }
    }

    private fun markKeyframeNeeded() {
        val shouldRequest = synchronized(codecLock) {
            val first = !needsKeyframe
            needsKeyframe = true
            first
        }
        if (shouldRequest) onKeyframeNeeded?.invoke()
    }

    companion object {
        private const val TAG = "WaystoneVideo"
        const val MAX_IN_FLIGHT = 4

        /** Splits Annex-B into NAL units without their start codes. */
        fun splitAnnexB(data: ByteArray): List<ByteArray> {
            val units = mutableListOf<ByteArray>()
            val count = data.size
            var index = 0
            var start = -1
            while (index + 2 < count) {
                if (data[index] == 0.toByte() && data[index + 1] == 0.toByte() && data[index + 2] == 1.toByte()) {
                    val end = if (index > 0 && data[index - 1] == 0.toByte()) index - 1 else index
                    if (start in 0 until end) {
                        units.add(data.copyOfRange(start, end))
                    }
                    index += 3
                    start = index
                } else {
                    index += 1
                }
            }
            if (start in 0 until count) {
                units.add(data.copyOfRange(start, count))
            }
            return units.filter { it.isNotEmpty() }
        }

        private fun withStartCode(nal: ByteArray): ByteArray {
            val out = ByteArray(4 + nal.size)
            out[3] = 1
            System.arraycopy(nal, 0, out, 4, nal.size)
            return out
        }

        private fun prependStartCodes(units: List<ByteArray>): ByteArray {
            val out = ByteArrayOutputStream(units.sumOf { it.size + 4 })
            val code = byteArrayOf(0, 0, 0, 1)
            for (unit in units) {
                out.write(code)
                out.write(unit)
            }
            return out.toByteArray()
        }

        private fun sameParameterSets(a: List<ByteArray>, b: List<ByteArray>): Boolean {
            if (a.size != b.size) return false
            for (i in a.indices) {
                if (!a[i].contentEquals(b[i])) return false
            }
            return true
        }

        private fun extractPictureSize(format: MediaFormat): Pair<Int, Int> {
            val hasCrop = format.containsKey("crop-left") &&
                format.containsKey("crop-right") &&
                format.containsKey("crop-top") &&
                format.containsKey("crop-bottom")
            return if (hasCrop) {
                val w = format.getInteger("crop-right") - format.getInteger("crop-left") + 1
                val h = format.getInteger("crop-bottom") - format.getInteger("crop-top") + 1
                w to h
            } else {
                val w = if (format.containsKey(MediaFormat.KEY_WIDTH)) format.getInteger(MediaFormat.KEY_WIDTH) else 0
                val h = if (format.containsKey(MediaFormat.KEY_HEIGHT)) format.getInteger(MediaFormat.KEY_HEIGHT) else 0
                w to h
            }
        }
    }
}
