package dev.waystone.app.protocol

import org.json.JSONArray
import org.json.JSONException
import org.json.JSONObject

/**
 * Kotlin mirror of `crates/waystone-proto/src/signal.rs`.
 *
 * Every message is one JSON object tagged by `type` with snake_case keys matching serde.
 */
object Wire {
    const val PROTOCOL_VERSION: Int = 1

    enum class Role(val wire: String) {
        HOST("host"),
        CONTROLLER("controller");

        companion object {
            fun fromWire(value: String): Role = entries.firstOrNull { it.wire == value }
                ?: throw JSONException("unknown role: $value")
        }
    }

    enum class Platform(val wire: String) {
        WINDOWS("windows"),
        MACOS("macos"),
        LINUX("linux"),
        IOS("ios"),
        IPADOS("ipados"),
        ANDROID("android"),
        OTHER("other");

        companion object {
            fun fromWire(value: String): Platform =
                entries.firstOrNull { it.wire == value } ?: OTHER
        }
    }

    enum class VideoCodec(val wire: String) {
        H264("h264"),
        H265("h265");

        companion object {
            fun fromWire(value: String): VideoCodec =
                entries.firstOrNull { it.wire == value } ?: H265
        }
    }

    data class Viewport(
        val width: Int,
        val height: Int,
        val dpi: Int,
        val fps: Int,
    ) {
        fun toJson(): JSONObject = JSONObject()
            .put("width", width)
            .put("height", height)
            .put("dpi", dpi)
            .put("fps", fps)
    }

    data class DeviceInfo(
        val id: String,
        val name: String,
        val platform: Platform,
        val role: Role,
        val publicKey: String,
    ) {
        fun toJson(): JSONObject = JSONObject()
            .put("id", id)
            .put("name", name)
            .put("platform", platform.wire)
            .put("role", role.wire)
            .put("public_key", publicKey)

        companion object {
            fun fromJson(obj: JSONObject): DeviceInfo = DeviceInfo(
                id = obj.getString("id"),
                name = obj.getString("name"),
                platform = Platform.fromWire(obj.getString("platform")),
                role = Role.fromWire(obj.getString("role")),
                publicKey = obj.getString("public_key"),
            )
        }
    }

    data class Identity(
        val id: String,
        val publicKey: String,
    ) {
        fun toJson(): JSONObject = JSONObject()
            .put("id", id)
            .put("public_key", publicKey)
    }

    data class TurnCredentials(
        val server: String,
        val username: String,
        val password: String,
        val relayBitrateCeiling: Int? = null,
    ) {
        companion object {
            fun fromJson(obj: JSONObject): TurnCredentials = TurnCredentials(
                server = obj.getString("server"),
                username = obj.getString("username"),
                password = obj.getString("password"),
                relayBitrateCeiling = if (obj.has("relay_bitrate_ceiling") && !obj.isNull("relay_bitrate_ceiling")) {
                    obj.getInt("relay_bitrate_ceiling")
                } else {
                    null
                },
            )
        }
    }

    data class PeerState(
        val device: DeviceInfo,
        val online: Boolean,
        val rttMs: Int? = null,
        val busy: Boolean = false,
    ) {
        companion object {
            fun fromJson(obj: JSONObject): PeerState = PeerState(
                device = DeviceInfo.fromJson(obj.getJSONObject("device")),
                online = obj.getBoolean("online"),
                rttMs = if (obj.has("rtt_ms") && !obj.isNull("rtt_ms")) obj.getInt("rtt_ms") else null,
                busy = obj.optBoolean("busy", false),
            )

            fun listFromJson(array: JSONArray): List<PeerState> =
                List(array.length()) { index -> fromJson(array.getJSONObject(index)) }
        }
    }

    enum class ByeReason(val wire: String) {
        USER_HANGUP("user_hangup"),
        LOCK_AND_EXIT("lock_and_exit"),
        NETWORK_TIMEOUT("network_timeout"),
        PEER_OFFLINE("peer_offline"),
        HOST_ERROR("host_error"),
        UNKNOWN("unknown");

        companion object {
            fun fromWire(value: String): ByeReason =
                entries.firstOrNull { it.wire == value } ?: UNKNOWN
        }
    }

    /** Must match `waystone_proto::identity::Challenge::signing_payload`. */
    fun signingPayload(serverId: String, nonce: String): ByteArray =
        "waystone-auth-v1|$serverId|$nonce".toByteArray(Charsets.UTF_8)

    sealed class RtcSignal {
        data class Offer(val sdp: String) : RtcSignal()
        data class Answer(val sdp: String) : RtcSignal()
        data class Ice(val candidate: String) : RtcSignal()
        data object IceComplete : RtcSignal()

        fun toJson(): JSONObject = when (this) {
            is Offer -> JSONObject().put("kind", "offer").put("sdp", sdp)
            is Answer -> JSONObject().put("kind", "answer").put("sdp", sdp)
            is Ice -> JSONObject()
                .put("kind", "ice")
                .put("candidate", candidate)
                .put("sdp_mid", JSONObject.NULL)
                .put("sdp_mline_index", JSONObject.NULL)
            IceComplete -> JSONObject().put("kind", "ice_complete")
        }

        companion object {
            fun fromJson(obj: JSONObject): RtcSignal = when (val kind = obj.getString("kind")) {
                "offer" -> Offer(obj.getString("sdp"))
                "answer" -> Answer(obj.getString("sdp"))
                "ice" -> Ice(obj.getString("candidate"))
                "ice_complete" -> IceComplete
                else -> throw JSONException("unknown rtc signal: $kind")
            }
        }
    }

    sealed class SessionControl {
        data class SetFrameRate(val fps: Int) : SessionControl()
        data class SetResolution(val width: Int, val height: Int) : SessionControl()
        data object RequestKeyframe : SessionControl()
        data class SetVideoEnabled(val enabled: Boolean) : SessionControl()
        data class SetAudioEnabled(val enabled: Boolean) : SessionControl()
        data class SelectDisplay(val index: Int) : SessionControl()

        fun toJson(): JSONObject = when (this) {
            is SetFrameRate -> JSONObject().put("kind", "set_frame_rate").put("fps", fps)
            is SetResolution -> JSONObject()
                .put("kind", "set_resolution")
                .put("width", width)
                .put("height", height)
            RequestKeyframe -> JSONObject().put("kind", "request_keyframe")
            is SetVideoEnabled -> JSONObject().put("kind", "set_video_enabled").put("enabled", enabled)
            is SetAudioEnabled -> JSONObject().put("kind", "set_audio_enabled").put("enabled", enabled)
            is SelectDisplay -> JSONObject().put("kind", "select_display").put("index", index)
        }

        companion object {
            fun fromJson(obj: JSONObject): SessionControl = when (val kind = obj.getString("kind")) {
                "set_frame_rate" -> SetFrameRate(obj.getInt("fps"))
                "set_resolution" -> SetResolution(obj.getInt("width"), obj.getInt("height"))
                "request_keyframe" -> RequestKeyframe
                "set_video_enabled" -> SetVideoEnabled(obj.getBoolean("enabled"))
                "set_audio_enabled" -> SetAudioEnabled(obj.getBoolean("enabled"))
                "select_display" -> SelectDisplay(obj.getInt("index"))
                else -> throw JSONException("unknown control: $kind")
            }
        }
    }

    sealed class ClientMessage {
        data class PairRequest(val token: String, val device: DeviceInfo) : ClientMessage()
        data class Auth(val identity: Identity, val signature: String, val name: String) : ClientMessage()
        data class SessionStart(
            val target: String,
            val viewport: Viewport,
            val audio: Boolean,
            val videoCodec: VideoCodec,
        ) : ClientMessage()
        data class RtcSignalMessage(val sessionId: String, val signal: RtcSignal) : ClientMessage()
        data class SessionControlMessage(val sessionId: String, val control: SessionControl) : ClientMessage()
        data class SessionBye(val sessionId: String, val reason: ByeReason) : ClientMessage()

        fun toJsonString(): String {
            val obj = when (this) {
                is PairRequest -> JSONObject()
                    .put("type", "pair_request")
                    .put("token", token)
                    .put("device", device.toJson())
                is Auth -> JSONObject()
                    .put("type", "auth")
                    .put("identity", identity.toJson())
                    .put("signature", signature)
                    .put("protocol_version", PROTOCOL_VERSION)
                    .put("name", name)
                is SessionStart -> JSONObject()
                    .put("type", "session_start")
                    .put("target", target)
                    .put("viewport", viewport.toJson())
                    .put("audio", audio)
                    .put("video_codec", videoCodec.wire)
                is RtcSignalMessage -> JSONObject()
                    .put("type", "rtc_signal")
                    .put("session_id", sessionId)
                    .put("signal", signal.toJson())
                is SessionControlMessage -> JSONObject()
                    .put("type", "session_control")
                    .put("session_id", sessionId)
                    .put("control", control.toJson())
                is SessionBye -> JSONObject()
                    .put("type", "session_bye")
                    .put("session_id", sessionId)
                    .put("reason", reason.wire)
            }
            return obj.toString()
        }
    }

    sealed class ServerMessage {
        data class Challenge(val nonce: String, val serverId: String) : ServerMessage()
        data class PairAccepted(val serverId: String) : ServerMessage()
        data class PairRejected(val reason: String) : ServerMessage()
        data class AuthOk(val peers: List<PeerState>, val turn: TurnCredentials?) : ServerMessage()
        data class AuthRejected(val reason: String) : ServerMessage()
        data class PeerUpdate(val peers: List<PeerState>) : ServerMessage()
        data class SessionAccepted(val sessionId: String, val peer: DeviceInfo, val videoCodec: VideoCodec) : ServerMessage()
        data class SessionRejected(val reason: String) : ServerMessage()
        data class RtcSignalMessage(val sessionId: String, val signal: RtcSignal) : ServerMessage()
        data class SessionControlMessage(val sessionId: String, val control: SessionControl) : ServerMessage()
        data class SessionEnded(val sessionId: String, val reason: ByeReason, val detail: String?) : ServerMessage()
        data object Pong : ServerMessage()
        data class Error(val message: String) : ServerMessage()

        companion object {
            fun decode(text: String): ServerMessage {
                val obj = JSONObject(text)
                return when (val type = obj.getString("type")) {
                    "challenge" -> Challenge(
                        nonce = obj.getString("nonce"),
                        serverId = obj.getString("server_id"),
                    )
                    "pair_accepted" -> PairAccepted(serverId = obj.getString("server_id"))
                    "pair_rejected" -> PairRejected(reason = obj.getString("reason"))
                    "auth_ok" -> AuthOk(
                        peers = PeerState.listFromJson(obj.getJSONArray("peers")),
                        turn = if (obj.has("turn") && !obj.isNull("turn")) {
                            TurnCredentials.fromJson(obj.getJSONObject("turn"))
                        } else {
                            null
                        },
                    )
                    "auth_rejected" -> AuthRejected(reason = obj.getString("reason"))
                    "peer_update" -> PeerUpdate(peers = PeerState.listFromJson(obj.getJSONArray("peers")))
                    "session_accepted" -> SessionAccepted(
                        sessionId = obj.getString("session_id"),
                        peer = DeviceInfo.fromJson(obj.getJSONObject("peer")),
                        videoCodec = if (obj.has("video_codec") && !obj.isNull("video_codec")) {
                            VideoCodec.fromWire(obj.getString("video_codec"))
                        } else {
                            VideoCodec.H265
                        },
                    )
                    "session_rejected" -> SessionRejected(reason = obj.getString("reason"))
                    "rtc_signal" -> RtcSignalMessage(
                        sessionId = obj.getString("session_id"),
                        signal = RtcSignal.fromJson(obj.getJSONObject("signal")),
                    )
                    "session_control" -> SessionControlMessage(
                        sessionId = obj.getString("session_id"),
                        control = SessionControl.fromJson(obj.getJSONObject("control")),
                    )
                    "session_ended" -> SessionEnded(
                        sessionId = obj.getString("session_id"),
                        reason = ByeReason.fromWire(obj.getString("reason")),
                        detail = if (obj.has("detail") && !obj.isNull("detail")) obj.getString("detail") else null,
                    )
                    "pong" -> Pong
                    "error" -> Error(message = obj.getString("message"))
                    else -> throw JSONException("unknown message type: $type")
                }
            }
        }
    }
}
