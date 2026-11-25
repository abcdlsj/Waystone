package dev.waystone.app.stream

import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import android.view.Surface
import dev.waystone.app.input.Size2D
import dev.waystone.app.media.AudioPlayer
import dev.waystone.app.media.VideoDecoder
import dev.waystone.app.protocol.Wire

/**
 * Transport -> VideoDecoder -> Surface, plus AudioPlayer, off the main thread.
 */
class MediaPipeline(
    codec: Wire.VideoCodec,
    private val audio: AudioPlayer,
) : MediaSink {
    private val mainHandler = Handler(Looper.getMainLooper())
    private val decoder = VideoDecoder(codec)

    private val lock = Any()
    private var lastMediaMs: Long = 0L
    private var lastPictureMs: Long? = null
    private var pictureSize: Size2D = Size2D.ZERO
    private var receivedFrames: Int = 0

    var onPictureSize: ((Size2D) -> Unit)? = null
    var onKeyframeNeeded: (() -> Unit)? = null
        set(value) {
            field = value
            decoder.onKeyframeNeeded = value
        }

    init {
        decoder.onFrameRendered = {
            synchronized(lock) {
                lastPictureMs = SystemClock.elapsedRealtime()
            }
        }
        decoder.onPictureSize = { w, h ->
            val size = Size2D(w.toFloat(), h.toFloat())
            val changed = synchronized(lock) {
                val diff = size != pictureSize
                pictureSize = size
                diff
            }
            if (changed) {
                mainHandler.post { onPictureSize?.invoke(size) }
            }
        }
    }

    fun attachSurface(surface: Surface?) {
        decoder.attachSurface(surface)
    }

    /** Seconds since the last decoded video frame was rendered. */
    val pictureAgeSeconds: Double
        get() = synchronized(lock) {
            val last = lastPictureMs ?: return Double.POSITIVE_INFINITY
            (SystemClock.elapsedRealtime() - last) / 1000.0
        }

    val hasPicture: Boolean
        get() = synchronized(lock) { lastPictureMs != null }

    val stats: Pair<Int, VideoDecoder.Stats>
        get() {
            val received = synchronized(lock) { receivedFrames }
            return received to decoder.currentStats
        }

    fun shutdown() {
        decoder.release()
        audio.stop()
    }

    override fun video(frame: ByteArray, keyframe: Boolean, rtpTime: Long) {
        synchronized(lock) {
            lastMediaMs = SystemClock.elapsedRealtime()
            receivedFrames += 1
        }
        decoder.decode(frame, rtpTime)
    }

    override fun audio(packet: ByteArray) {
        synchronized(lock) {
            lastMediaMs = SystemClock.elapsedRealtime()
        }
        audio.enqueue(packet)
    }
}
