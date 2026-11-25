package dev.waystone.app.pool

import android.os.Build
import android.util.Log
import dev.waystone.app.protocol.Wire
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch
import kotlin.math.min
import kotlin.math.pow

/** A machine in the private pool that can be streamed. */
data class Host(
    val id: String,
    val name: String,
    val platform: Wire.Platform,
    val online: Boolean,
    val busy: Boolean,
    val rttMs: Int?,
) {
    constructor(peer: Wire.PeerState) : this(
        id = peer.device.id,
        name = peer.device.name,
        platform = peer.device.platform,
        online = peer.online,
        busy = peer.busy,
        rttMs = peer.rttMs,
    )
}

/** Receives the messages of one stream session. */
interface SessionMessageHandler {
    fun sessionAccepted(id: String, codec: Wire.VideoCodec)
    fun sessionRejected(reason: String)
    fun received(signal: Wire.RtcSignal)
    fun received(control: Wire.SessionControl)
    fun sessionEnded(reason: Wire.ByeReason, detail: String?)
    fun signalingLost()
}

/**
 * Long-lived authenticated connection to one Waystone server.
 *
 * Keeps the host list fresh, reconnects with capped exponential backoff,
 * handles one-shot pairing, and routes session messages.
 */
class PoolConnection(
    val instance: Instance,
    private val identity: DeviceIdentity,
    private val scope: CoroutineScope,
    private var pairingToken: String? = null,
) {
    sealed class Status {
        data object Connecting : Status()
        data object Online : Status()
        data class Offline(val reason: String) : Status()
        data class Rejected(val reason: String) : Status()
    }

    val id: String get() = instance.id

    private val _status = MutableStateFlow<Status>(Status.Connecting)
    val status: StateFlow<Status> = _status.asStateFlow()

    private val _hosts = MutableStateFlow<List<Host>>(emptyList())
    val hosts: StateFlow<List<Host>> = _hosts.asStateFlow()

    val isOnline: Boolean get() = _status.value is Status.Online

    var onPaired: (() -> Unit)? = null
    var onPairingFailed: ((String) -> Unit)? = null

    var turn: Wire.TurnCredentials? = null
        private set

    private var client: SignalingClient? = null
    private var challenge: Pair<String, String>? = null
    private var retryJob: Job? = null
    private var failures = 0
    private var running = false

    private var session: SessionMessageHandler? = null
    private var sessionId: String? = null

    fun start() {
        if (running) return
        running = true
        failures = 0
        connect()
    }

    fun stop() {
        running = false
        retryJob?.cancel()
        retryJob = null
        client?.close()
        client = null
        if (_status.value is Status.Rejected) return
        _status.value = Status.Offline("已暂停")
    }

    fun reconnectNow() {
        if (!running) {
            start()
            return
        }
        if (_status.value is Status.Online) return
        if (client != null && _status.value is Status.Connecting) return
        retryJob?.cancel()
        retryJob = null
        failures = 0
        connect()
    }

    fun requestSession(
        host: Host,
        viewport: Wire.Viewport,
        audio: Boolean,
        codec: Wire.VideoCodec,
        handler: SessionMessageHandler,
    ) {
        session = handler
        sessionId = null
        send(
            Wire.ClientMessage.SessionStart(
                target = host.id,
                viewport = viewport,
                audio = audio,
                videoCodec = codec,
            )
        )
    }

    fun releaseSession(handler: SessionMessageHandler) {
        if (session !== handler) return
        session = null
        sessionId = null
    }

    fun host(id: String): Host? = _hosts.value.firstOrNull { it.id == id }

    fun send(message: Wire.ClientMessage) {
        client?.send(message)
    }

    private fun connect() {
        client?.close()
        challenge = null
        _status.value = Status.Connecting
        val newClient = try {
            SignalingClient(endpoint = instance.endpoint, fingerprint = instance.fingerprint)
        } catch (_: Exception) {
            fail("服务器地址无效")
            return
        }
        client = newClient
        newClient.start { event ->
            if (newClient !== client) return@start
            handle(event)
        }
    }

    private fun handle(event: SignalingClient.Event) {
        when (event) {
            SignalingClient.Event.Opened -> Unit
            is SignalingClient.Event.Message -> handle(event.message)
            is SignalingClient.Event.Closed -> {
                Log.i(TAG, "connection to ${instance.endpoint} closed: ${event.reason}")
                client = null
                session?.signalingLost()
                if (pairingToken != null) {
                    pairingToken = null
                    onPairingFailed?.invoke("无法连接到服务器：${event.reason}")
                    running = false
                    _status.value = Status.Offline(event.reason)
                    return
                }
                if (_status.value is Status.Rejected) return
                fail(describe(event.reason))
            }
        }
    }

    private fun handle(message: Wire.ServerMessage) {
        when (message) {
            is Wire.ServerMessage.Challenge -> {
                challenge = message.nonce to message.serverId
                val token = pairingToken
                if (token != null) {
                    send(
                        Wire.ClientMessage.PairRequest(
                            token = token,
                            device = identity.info(name = deviceName, platform = Wire.Platform.ANDROID),
                        )
                    )
                } else {
                    authenticate()
                }
            }
            is Wire.ServerMessage.PairAccepted -> {
                pairingToken = null
                onPaired?.invoke()
                authenticate()
            }
            is Wire.ServerMessage.PairRejected -> {
                pairingToken = null
                running = false
                _status.value = Status.Rejected(message.reason)
                onPairingFailed?.invoke(describePairing(message.reason))
                client?.close()
                client = null
            }
            is Wire.ServerMessage.AuthOk -> {
                failures = 0
                turn = message.turn
                applyPeers(message.peers)
                _status.value = Status.Online
            }
            is Wire.ServerMessage.AuthRejected -> {
                client?.close()
                client = null
                if (message.reason.contains("replaced")) {
                    fail("已在其他地方连接")
                } else {
                    running = false
                    _status.value = Status.Rejected(describeRejection(message.reason))
                }
            }
            is Wire.ServerMessage.PeerUpdate -> applyPeers(message.peers)
            is Wire.ServerMessage.SessionAccepted -> {
                sessionId = message.sessionId
                session?.sessionAccepted(message.sessionId, message.videoCodec)
            }
            is Wire.ServerMessage.SessionRejected -> session?.sessionRejected(message.reason)
            is Wire.ServerMessage.RtcSignalMessage -> {
                if (message.sessionId == sessionId) session?.received(message.signal)
            }
            is Wire.ServerMessage.SessionControlMessage -> {
                if (message.sessionId == sessionId) session?.received(message.control)
            }
            is Wire.ServerMessage.SessionEnded -> {
                if (message.sessionId == sessionId) {
                    session?.sessionEnded(message.reason, message.detail)
                }
            }
            Wire.ServerMessage.Pong -> Unit
            is Wire.ServerMessage.Error -> Log.e(TAG, "server error: ${message.message}")
        }
    }

    private fun authenticate() {
        val current = challenge ?: return
        challenge = null
        try {
            val payload = Wire.signingPayload(serverId = current.second, nonce = current.first)
            val signature = identity.sign(payload)
            send(
                Wire.ClientMessage.Auth(
                    identity = identity.wireIdentity,
                    signature = signature,
                    name = deviceName,
                )
            )
        } catch (e: Exception) {
            fail("无法签名身份：${e.message}")
        }
    }

    private fun applyPeers(peers: List<Wire.PeerState>) {
        _hosts.value = peers
            .filter { it.device.role == Wire.Role.HOST }
            .map(::Host)
            .sortedWith(compareBy<Host>({ if (it.online) 0 else 1 }, { it.name }))
    }

    private fun fail(reason: String) {
        client?.close()
        client = null
        _status.value = Status.Offline(reason)
        _hosts.value = _hosts.value.map { it.copy(online = false) }
        if (!running) return
        failures += 1
        val delaySec = min(2.0.pow((failures - 1).toDouble()), 30.0)
        retryJob?.cancel()
        retryJob = scope.launch {
            delay((delaySec * 1000).toLong())
            if (running) connect()
        }
    }

    companion object {
        private const val TAG = "WaystonePool"

        val deviceName: String
            get() {
                val model = Build.MODEL?.trim().orEmpty()
                val brand = Build.MANUFACTURER?.trim().orEmpty()
                return when {
                    model.isEmpty() -> "Android"
                    brand.isEmpty() || model.startsWith(brand, ignoreCase = true) -> model
                    else -> "$brand $model"
                }
            }

        private fun describe(reason: String): String {
            val lowered = reason.lowercase()
            if (lowered.contains("cancel") || lowered.contains("certificate") || lowered.contains("fingerprint")) {
                return "证书校验失败，请重新扫码添加"
            }
            if (lowered.contains("timed out") || lowered.contains("timeout") || lowered.contains("超时")) {
                return "连接服务器超时"
            }
            if (lowered.contains("offline") || lowered.contains("unable to resolve") || lowered.contains("network is unreachable")) {
                return "手机没有网络"
            }
            return "无法连接服务器"
        }

        private fun describePairing(reason: String): String {
            if (reason.contains("expired") || reason.contains("used") || reason.contains("invalid")) {
                return "配对码已失效，请在面板上重新生成"
            }
            return "配对失败：$reason"
        }

        private fun describeRejection(reason: String): String {
            if (reason.contains("not paired") || reason.contains("revoked")) {
                return "此手机已被移出设备池，请重新扫码添加"
            }
            return reason
        }
    }
}
