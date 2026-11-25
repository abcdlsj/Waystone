package dev.waystone.app.protocol

import java.net.URI
import java.net.URLDecoder

/**
 * The `waystone://pair?ep=...&turn=...&fp=...&tk=...` link rendered by the panel.
 *
 * Mirrors `waystone_proto::pairing::PairingUri`.
 */
data class PairingLink(
    val endpoint: String,
    val turn: String,
    val fingerprint: String,
    val token: String,
) {
    companion object {
        fun parse(text: String): PairingLink? {
            val trimmed = text.trim()
            val uri = try {
                URI(trimmed)
            } catch (_: Exception) {
                return null
            }
            if (uri.scheme != "waystone" || uri.host != "pair") return null
            val rawQuery = uri.rawQuery ?: return null
            val values = mutableMapOf<String, String>()
            for (pair in rawQuery.split('&')) {
                val eq = pair.indexOf('=')
                if (eq <= 0) continue
                val key = URLDecoder.decode(pair.substring(0, eq), Charsets.UTF_8.name())
                val value = URLDecoder.decode(pair.substring(eq + 1), Charsets.UTF_8.name())
                if (value.isNotEmpty()) values[key] = value
            }
            val endpoint = values["ep"] ?: return null
            val turn = values["turn"] ?: return null
            val fingerprint = values["fp"]?.lowercase() ?: return null
            if (fingerprint.length != 64 || !fingerprint.all { it in '0'..'9' || it in 'a'..'f' }) {
                return null
            }
            val token = values["tk"] ?: return null
            if (token.length < 16) return null
            return PairingLink(
                endpoint = endpoint,
                turn = turn,
                fingerprint = fingerprint,
                token = token,
            )
        }
    }
}
