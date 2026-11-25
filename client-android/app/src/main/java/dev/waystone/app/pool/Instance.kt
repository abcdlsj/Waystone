package dev.waystone.app.pool

import dev.waystone.app.protocol.PairingLink
import org.json.JSONObject

/**
 * A Waystone server this phone is enrolled with.
 *
 * The pinned certificate fingerprint doubles as the stable identity.
 */
data class Instance(
    val endpoint: String,
    val turn: String,
    val fingerprint: String,
    val label: String,
) {
    val id: String get() = fingerprint

    fun toJson(): JSONObject = JSONObject()
        .put("endpoint", endpoint)
        .put("turn", turn)
        .put("fingerprint", fingerprint)
        .put("label", label)

    companion object {
        fun fromLink(link: PairingLink): Instance = Instance(
            endpoint = link.endpoint,
            turn = link.turn,
            fingerprint = link.fingerprint,
            label = defaultLabel(link.endpoint),
        )

        fun fromJson(obj: JSONObject): Instance = Instance(
            endpoint = obj.getString("endpoint"),
            turn = obj.getString("turn"),
            fingerprint = obj.getString("fingerprint"),
            label = obj.optString("label").ifEmpty { defaultLabel(obj.getString("endpoint")) },
        )

        /** The host part of the endpoint, which is what people recognize. */
        fun defaultLabel(endpoint: String): String {
            if (endpoint.startsWith("[")) {
                val close = endpoint.indexOf(']')
                if (close > 1) return endpoint.substring(1, close)
            }
            val colon = endpoint.lastIndexOf(':')
            return if (colon > 0) endpoint.substring(0, colon) else endpoint
        }
    }
}
