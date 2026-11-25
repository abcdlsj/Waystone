package dev.waystone.app.stream

import dev.waystone.app.protocol.InputEvent
import dev.waystone.app.protocol.Wire
import org.json.JSONObject

/** Receives media directly on the Rust transport thread. */
interface MediaSink {
    fun video(frame: ByteArray, keyframe: Boolean, rtpTime: Long)
    fun audio(packet: ByteArray)
}

/**
 * Kotlin face of the shared Rust WebRTC transport (`crates/waystone-rtc`) via JNI.
 */
class Transport(
    configuration: Configuration,
    private val media: MediaSink,
    private val onControl: (ControlEvent) -> Unit,
) {
    sealed class ControlEvent {
        data class LocalOffer(val sdp: String) : ControlEvent()
        data class LocalCandidate(val candidate: String) : ControlEvent()
        data object Connected : ControlEvent()
        data object Disconnected : ControlEvent()
        data class Path(val relayed: Boolean, val rttMs: Int?) : ControlEvent()
        data class Closed(val reason: String) : ControlEvent()
    }

    data class Configuration(
        val audio: Boolean,
        val codec: Wire.VideoCodec,
        val stun: String?,
        val turn: Wire.TurnCredentials?,
    )

    interface NativeCallback {
        fun onNativeEvent(kind: Int, data: ByteArray)
    }

    @Volatile
    private var handle: Long = 0L

    init {
        ensureLoaded()
        val config = JSONObject()
            .put("role", "controller")
            .put("audio", configuration.audio)
            .put("video_codec", configuration.codec.wire)
            .put("bind", "0.0.0.0:0")
        if (configuration.stun != null) {
            config.put("stun", configuration.stun)
        }
        if (configuration.turn != null) {
            val turnObj = JSONObject()
                .put("server", configuration.turn.server)
                .put("username", configuration.turn.username)
                .put("password", configuration.turn.password)
            if (configuration.turn.relayBitrateCeiling != null) {
                turnObj.put("relay_bitrate_ceiling", configuration.turn.relayBitrateCeiling)
            }
            config.put("turn", turnObj)
        }

        val callback = object : NativeCallback {
            override fun onNativeEvent(kind: Int, data: ByteArray) {
                dispatch(kind, data)
            }
        }
        handle = try {
            nativeStart(config.toString(), callback)
        } catch (e: Exception) {
            throw RuntimeException("无法启动传输：${e.message}", e)
        }
        if (handle == 0L) {
            throw RuntimeException("无法启动传输：引擎初始化失败")
        }
    }

    fun acceptAnswer(sdp: String) {
        val h = handle
        if (h != 0L) nativeAcceptAnswer(h, sdp)
    }

    fun addCandidate(candidate: String) {
        val h = handle
        if (h != 0L) nativeAddCandidate(h, candidate)
    }

    fun send(event: InputEvent) {
        val h = handle
        if (h != 0L) nativeSendInput(h, event.isReliable, event.encoded())
    }

    fun stop() {
        val h = synchronized(this) {
            val current = handle
            handle = 0L
            current
        }
        if (h != 0L) {
            nativeStop(h)
        }
    }

    private fun dispatch(kind: Int, bytes: ByteArray) {
        when (kind) {
            WS_EVENT_VIDEO -> {
                val (header, payload) = splitMedia(bytes) ?: return
                media.video(payload, header.keyframe, header.rtpTime)
            }
            WS_EVENT_AUDIO -> {
                val (_, payload) = splitMedia(bytes) ?: return
                media.audio(payload)
            }
            WS_EVENT_CONTROL -> {
                parseControl(bytes)?.let(onControl)
            }
        }
    }

    private external fun nativeStart(configJson: String, callback: NativeCallback): Long
    private external fun nativeStop(handle: Long)
    private external fun nativeAcceptAnswer(handle: Long, sdp: String)
    private external fun nativeAddCandidate(handle: Long, candidate: String)
    private external fun nativeSendInput(handle: Long, reliable: Boolean, data: ByteArray)

    data class MediaHeader(
        val flags: Int,
        val rtpTime: Long,
    ) {
        val keyframe: Boolean get() = (flags and 1) != 0
    }

    companion object {
        private const val WS_EVENT_CONTROL = 0
        private const val WS_EVENT_VIDEO = 1
        private const val WS_EVENT_AUDIO = 2
        private const val MEDIA_HEADER_LEN = 16

        @Volatile
        private var loaded = false

        private fun ensureLoaded() {
            if (loaded) return
            synchronized(this) {
                if (loaded) return
                try {
                    System.loadLibrary("waystone_rtc")
                    loaded = true
                } catch (e: UnsatisfiedLinkError) {
                    throw RuntimeException(
                        "缺少 libwaystone_rtc.so，请先运行 client-android/scripts/build-rust.sh",
                        e,
                    )
                }
            }
        }

        fun splitMedia(bytes: ByteArray): Pair<MediaHeader, ByteArray>? {
            if (bytes.size < MEDIA_HEADER_LEN) return null
            var time = 0L
            for (i in 2 until 10) {
                time = (time shl 8) or (bytes[i].toLong() and 0xffL)
            }
            val header = MediaHeader(flags = bytes[1].toInt() and 0xff, rtpTime = time)
            val payload = bytes.copyOfRange(MEDIA_HEADER_LEN, bytes.size)
            return header to payload
        }

        fun parseControl(bytes: ByteArray): ControlEvent? {
            return runCatching {
                val json = JSONObject(String(bytes, Charsets.UTF_8))
                when (json.optString("kind")) {
                    "local_offer" -> json.optString("sdp").takeIf { it.isNotEmpty() }?.let { ControlEvent.LocalOffer(it) }
                    "local_candidate" -> json.optString("candidate").takeIf { it.isNotEmpty() }?.let { ControlEvent.LocalCandidate(it) }
                    "connected" -> ControlEvent.Connected
                    "disconnected" -> ControlEvent.Disconnected
                    "path_update" -> ControlEvent.Path(
                        relayed = json.optBoolean("relayed", false),
                        rttMs = if (json.has("rttMs") && !json.isNull("rttMs")) json.getInt("rttMs") else null,
                    )
                    "closed" -> ControlEvent.Closed(json.optString("reason", ""))
                    else -> null
                }
            }.getOrNull()
        }
    }
}
