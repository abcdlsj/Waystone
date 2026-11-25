package dev.waystone.app.stream

import android.content.Context
import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import android.util.Log
import android.view.Surface
import dev.waystone.app.input.HotkeyPreset
import dev.waystone.app.input.InputController
import dev.waystone.app.input.PointerOutput
import dev.waystone.app.media.AudioPlayer
import dev.waystone.app.media.VideoCodecSupport
import dev.waystone.app.pool.Host
import dev.waystone.app.pool.PoolConnection
import dev.waystone.app.pool.SessionMessageHandler
import dev.waystone.app.protocol.Wire
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import java.util.UUID
import kotlin.math.max
import kotlin.math.min
import kotlin.math.roundToInt

/**
 * One stream session from a host: negotiation, media, input, health and teardown.
 *
 * Mirrors iOS `StreamSession` 1:1.
 */
class StreamSession(
    private val context: Context,
    val host: Host,
    private val pool: PoolConnection,
    private val preferences: StreamPreferences,
    private val scope: CoroutineScope,
    haptics: (PointerOutput.Haptic) -> Unit,
) : SessionMessageHandler {
    sealed class Stage {
        data object Connecting : Stage()
        data object Live : Stage()
        data class Stalled(val sinceMs: Long) : Stage()
        data class Ended(val ending: Ending) : Stage()
    }

    data class Ending(
        val title: String,
        val message: String? = null,
    ) {
        companion object {
            val HUNG_UP = Ending(title = "已断开", message = null)
        }
    }

    data class PathInfo(
        val relayed: Boolean = false,
        val rttMs: Int? = null,
    )

    val id: String = UUID.randomUUID().toString()
    val input: InputController = InputController(initialMode = preferences.touchMode.value, haptics = haptics)

    private val _stage = MutableStateFlow<Stage>(Stage.Connecting)
    val stage: StateFlow<Stage> = _stage.asStateFlow()

    private val _path = MutableStateFlow(PathInfo())
    val path: StateFlow<PathInfo> = _path.asStateFlow()

    private val _progress = MutableStateFlow("正在连接…")
    val progress: StateFlow<String> = _progress.asStateFlow()

    private val _liveFps = MutableStateFlow(0)
    val liveFps: StateFlow<Int> = _liveFps.asStateFlow()

    private val mainHandler = Handler(Looper.getMainLooper())
    private val codec = VideoCodecSupport.bestDecodable()
    private val audio = AudioPlayer(context)
    private val pipeline = MediaPipeline(codec = codec, audio = audio)
    private var transport: Transport? = null
    private var sessionId: String? = null
    private val outbox = mutableListOf<Wire.RtcSignal>()
    private var startedAtMs: Long = 0L
    private var healthJob: Job? = null
    private var lastKeyframeRequestMs: Long = 0L
    private var lastFpsDecoded = 0
    private var lastFpsAtMs = 0L

    val isEnded: Boolean get() = _stage.value is Stage.Ended

    init {
        input.sink = { event -> transport?.send(event) }
        pipeline.onPictureSize = { size -> input.videoSizeChanged(size) }
        pipeline.onKeyframeNeeded = {
            mainHandler.post { requestKeyframe() }
        }
    }

    fun attachSurface(surface: Surface?) {
        pipeline.attachSurface(surface)
    }

    fun start() {
        startedAtMs = SystemClock.elapsedRealtime()
        lastFpsDecoded = 0
        lastFpsAtMs = startedAtMs
        val viewport = viewport(context, preferences.resolution.value, preferences.frameRate.value.fps)
        val audioEnabled = preferences.sound.value != StreamPreferences.Sound.OFF
        transport = try {
            Transport(
                configuration = Transport.Configuration(
                    audio = audioEnabled,
                    codec = codec,
                    stun = pool.turn?.server ?: pool.instance.turn,
                    turn = pool.turn,
                ),
                media = pipeline,
                onControl = { event ->
                    mainHandler.post { handleControl(event) }
                },
            )
        } catch (e: Exception) {
            end(Ending(title = "无法开始串流", message = e.message))
            return
        }
        if (audioEnabled) {
            audio.start(forceSound = preferences.sound.value == StreamPreferences.Sound.ON)
        }
        pool.requestSession(
            host = host,
            viewport = viewport,
            audio = audioEnabled,
            codec = codec,
            handler = this,
        )
        _progress.value = "等待响应…"
        healthJob = scope.launch {
            while (isActive && !isEnded) {
                delay(500)
                checkHealth()
            }
        }
    }

    fun hangUp(lock: Boolean) {
        if (isEnded) return
        input.releaseAll()
        val currentSessionId = sessionId
        if (currentSessionId != null) {
            pool.send(
                Wire.ClientMessage.SessionBye(
                    sessionId = currentSessionId,
                    reason = if (lock) Wire.ByeReason.LOCK_AND_EXIT else Wire.ByeReason.USER_HANGUP,
                )
            )
        }
        end(Ending.HUNG_UP)
    }

    fun setFrameRate(rate: StreamPreferences.FrameRate) {
        preferences.setFrameRate(rate)
        sendControl(Wire.SessionControl.SetFrameRate(rate.fps))
    }

    fun setResolution(resolution: StreamPreferences.Resolution) {
        preferences.setResolution(resolution)
        val (w, h) = targetSize(context, resolution)
        sendControl(Wire.SessionControl.SetResolution(w, h))
    }

    fun setSound(sound: StreamPreferences.Sound) {
        val wasOff = preferences.sound.value == StreamPreferences.Sound.OFF
        preferences.setSound(sound)
        when (sound) {
            StreamPreferences.Sound.OFF -> {
                audio.setMuted(true)
                sendControl(Wire.SessionControl.SetAudioEnabled(false))
            }
            StreamPreferences.Sound.AUTO, StreamPreferences.Sound.ON -> {
                val force = sound == StreamPreferences.Sound.ON
                audio.start(forceSound = force)
                audio.setForceSound(force)
                audio.setMuted(false)
                if (wasOff) sendControl(Wire.SessionControl.SetAudioEnabled(true))
            }
        }
    }

    fun setTouchMode(mode: InputController.Mode) {
        preferences.setTouchMode(mode)
        input.setMode(mode)
    }

    fun setPreset(preset: HotkeyPreset) {
        if (preset.id == preferences.presetId.value) return
        input.releaseAll()
        preferences.setPresetId(preset.id)
    }

    fun requestKeyframe() {
        val now = SystemClock.elapsedRealtime()
        if (now - lastKeyframeRequestMs <= 500L) return
        lastKeyframeRequestMs = now
        sendControl(Wire.SessionControl.RequestKeyframe)
    }

    // SessionMessageHandler

    override fun sessionAccepted(id: String, codec: Wire.VideoCodec) {
        sessionId = id
        _progress.value = "建立连接…"
        for (signal in outbox) {
            pool.send(Wire.ClientMessage.RtcSignalMessage(id, signal))
        }
        outbox.clear()
        if (preferences.frameRate.value != StreamPreferences.FrameRate.SMOOTH) {
            sendControl(Wire.SessionControl.SetFrameRate(preferences.frameRate.value.fps))
        }
    }

    override fun sessionRejected(reason: String) {
        val message = if (reason.contains("in use") || reason.contains("busy")) {
            "${host.name} 正在被其他设备使用"
        } else {
            reason
        }
        end(Ending(title = "无法连接", message = message))
    }

    override fun received(signal: Wire.RtcSignal) {
        when (signal) {
            is Wire.RtcSignal.Answer -> transport?.acceptAnswer(signal.sdp)
            is Wire.RtcSignal.Ice -> transport?.addCandidate(signal.candidate)
            is Wire.RtcSignal.Offer, Wire.RtcSignal.IceComplete -> Unit
        }
    }

    override fun received(control: Wire.SessionControl) = Unit

    override fun sessionEnded(reason: Wire.ByeReason, detail: String?) {
        when (reason) {
            Wire.ByeReason.USER_HANGUP, Wire.ByeReason.LOCK_AND_EXIT ->
                end(Ending("会话已结束", "${host.name} 结束了这次连接。"))
            Wire.ByeReason.PEER_OFFLINE ->
                end(Ending("电脑已离线", "${host.name} 当前已离线或休眠。"))
            Wire.ByeReason.NETWORK_TIMEOUT ->
                end(Ending("网络中断", "与 ${host.name} 的连接超时。"))
            Wire.ByeReason.HOST_ERROR, Wire.ByeReason.UNKNOWN ->
                end(Ending("电脑端出错", detail ?: "${host.name} 无法继续串流。"))
        }
    }

    override fun signalingLost() {
        Log.i(TAG, "signaling lost during session")
    }

    private fun handleControl(event: Transport.ControlEvent) {
        if (isEnded) return
        when (event) {
            is Transport.ControlEvent.LocalOffer -> relay(Wire.RtcSignal.Offer(event.sdp))
            is Transport.ControlEvent.LocalCandidate -> relay(Wire.RtcSignal.Ice(event.candidate))
            Transport.ControlEvent.Connected -> {
                _progress.value = "等待画面…"
                requestKeyframe()
            }
            Transport.ControlEvent.Disconnected -> Log.i(TAG, "ICE disconnected")
            is Transport.ControlEvent.Path -> {
                _path.value = PathInfo(
                    relayed = event.relayed,
                    rttMs = event.rttMs ?: _path.value.rttMs,
                )
            }
            is Transport.ControlEvent.Closed -> end(Ending("连接已关闭", event.reason))
        }
    }

    private fun relay(signal: Wire.RtcSignal) {
        val id = sessionId
        if (id != null) {
            pool.send(Wire.ClientMessage.RtcSignalMessage(id, signal))
        } else {
            outbox.add(signal)
        }
    }

    private fun sendControl(control: Wire.SessionControl) {
        val id = sessionId ?: return
        pool.send(Wire.ClientMessage.SessionControlMessage(id, control))
    }

    private fun checkHealth() {
        updateFps()
        val age = pipeline.pictureAgeSeconds
        val now = SystemClock.elapsedRealtime()
        when (_stage.value) {
            Stage.Connecting -> {
                if (pipeline.hasPicture) {
                    _stage.value = Stage.Live
                } else if ((now - startedAtMs) / 1000.0 > FIRST_PICTURE_TIMEOUT_SEC) {
                    end(attributeFailure(firstPicture = true))
                }
            }
            Stage.Live -> {
                if (age > BANNER_AFTER_SEC) {
                    _stage.value = Stage.Stalled(now - (age * 1000).toLong())
                    requestKeyframe()
                } else if (age > 1.5) {
                    requestKeyframe()
                }
            }
            is Stage.Stalled -> {
                if (age < 1.0) {
                    _stage.value = Stage.Live
                } else if (age > GIVE_UP_AFTER_SEC) {
                    end(attributeFailure(firstPicture = false))
                } else {
                    requestKeyframe()
                }
            }
            is Stage.Ended -> Unit
        }
    }

    private fun attributeFailure(firstPicture: Boolean): Ending {
        if (!pool.isOnline) {
            return Ending("服务器连接超时", "无法连接服务器，请检查网络。")
        }
        val current = pool.host(host.id)
        if (current != null && !current.online) {
            return Ending("主机已离线", "${host.name} 已离线。")
        }
        if (_path.value.relayed) {
            return Ending("中继超时", "中继连接未恢复，请检查服务器网络。")
        }
        return if (firstPicture) {
            Ending("连接超时", "未收到画面，可能被网络阻断。")
        } else {
            Ending("网络中断", "画面中断超过 ${GIVE_UP_AFTER_SEC.toInt()} 秒。")
        }
    }

    private fun end(ending: Ending) {
        if (isEnded) return
        Log.i(TAG, "session ended: ${ending.title} ${ending.message.orEmpty()}")
        healthJob?.cancel()
        healthJob = null
        transport?.stop()
        transport = null
        pipeline.shutdown()
        pool.releaseSession(this)
        _stage.value = Stage.Ended(ending)
    }

    private fun updateFps() {
        val now = SystemClock.elapsedRealtime()
        val elapsedSec = (now - lastFpsAtMs) / 1000.0
        if (elapsedSec < 0.95) return
        val decoded = pipeline.stats.second.decoded
        val delta = max(0, decoded - lastFpsDecoded)
        _liveFps.value = (delta / elapsedSec).roundToInt()
        lastFpsDecoded = decoded
        lastFpsAtMs = now
    }

    companion object {
        private const val TAG = "WaystoneSession"
        const val BANNER_AFTER_SEC = 3.0
        const val GIVE_UP_AFTER_SEC = 12.0
        const val FIRST_PICTURE_TIMEOUT_SEC = 20.0

        fun targetSize(context: Context, resolution: StreamPreferences.Resolution): Pair<Int, Int> {
            resolution.maxSize?.let { return it }
            val metrics = context.resources.displayMetrics
            return max(metrics.widthPixels, metrics.heightPixels) to
                min(metrics.widthPixels, metrics.heightPixels)
        }

        fun viewport(context: Context, resolution: StreamPreferences.Resolution, fps: Int): Wire.Viewport {
            val (w, h) = targetSize(context, resolution)
            val dpi = context.resources.displayMetrics.densityDpi.coerceIn(120, 640)
            return Wire.Viewport(width = w, height = h, dpi = dpi, fps = fps)
        }
    }
}
