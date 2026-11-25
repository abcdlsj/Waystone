package dev.waystone.app.pool

import android.os.Handler
import android.os.Looper
import android.util.Log
import dev.waystone.app.protocol.Wire
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.Response
import okhttp3.WebSocket
import okhttp3.WebSocketListener
import java.security.MessageDigest
import java.security.SecureRandom
import java.security.cert.CertificateException
import java.security.cert.X509Certificate
import java.util.concurrent.TimeUnit
import javax.net.ssl.HostnameVerifier
import javax.net.ssl.SSLContext
import javax.net.ssl.X509TrustManager

/**
 * One WSS connection to a Waystone server with leaf certificate SHA-256 pinning.
 *
 * The server uses a self-signed certificate, so trust comes solely from the
 * SHA-256 fingerprint carried by the pairing link instead of a CA chain.
 */
class SignalingClient(
    endpoint: String,
    fingerprint: String,
) {
    sealed class Event {
        data object Opened : Event()
        data class Message(val message: Wire.ServerMessage) : Event()
        data class Closed(val reason: String) : Event()
    }

    private val url = "wss://$endpoint/ws"
    private val expectedFingerprint = fingerprint.lowercase()
    private val mainHandler = Handler(Looper.getMainLooper())
    private val httpClient: OkHttpClient
    private var webSocket: WebSocket? = null
    private var onEvent: ((Event) -> Unit)? = null
    @Volatile
    private var closed = false

    init {
        val trustManager = PinnedCertTrustManager(expectedFingerprint)
        val sslContext = SSLContext.getInstance("TLS").apply {
            init(null, arrayOf(trustManager), SecureRandom())
        }
        val hostnameVerifier = HostnameVerifier { _, session ->
            val leaf = session.peerCertificates.firstOrNull() as? X509Certificate ?: return@HostnameVerifier false
            sha256Hex(leaf.encoded) == expectedFingerprint
        }
        httpClient = OkHttpClient.Builder()
            .connectTimeout(10, TimeUnit.SECONDS)
            .readTimeout(0, TimeUnit.MILLISECONDS)
            .pingInterval(20, TimeUnit.SECONDS)
            .sslSocketFactory(sslContext.socketFactory, trustManager)
            .hostnameVerifier(hostnameVerifier)
            .build()
    }

    fun start(onEvent: (Event) -> Unit) {
        this.onEvent = onEvent
        val request = Request.Builder().url(url).build()
        webSocket = httpClient.newWebSocket(request, object : WebSocketListener() {
            override fun onOpen(webSocket: WebSocket, response: Response) {
                mainHandler.post {
                    if (!closed) this@SignalingClient.onEvent?.invoke(Event.Opened)
                }
            }

            override fun onMessage(webSocket: WebSocket, text: String) {
                val decoded = try {
                    Wire.ServerMessage.decode(text)
                } catch (e: Exception) {
                    // An unknown message from a newer server is not fatal.
                    Log.i(TAG, "ignoring message: ${e.message}")
                    return
                }
                mainHandler.post {
                    if (!closed) this@SignalingClient.onEvent?.invoke(Event.Message(decoded))
                }
            }

            override fun onClosing(webSocket: WebSocket, code: Int, reason: String) {
                webSocket.close(1000, null)
                val text = reason.ifEmpty { "closed ($code)" }
                mainHandler.post { finish(text) }
            }

            override fun onClosed(webSocket: WebSocket, code: Int, reason: String) {
                val text = reason.ifEmpty { "closed ($code)" }
                mainHandler.post { finish(text) }
            }

            override fun onFailure(webSocket: WebSocket, t: Throwable, response: Response?) {
                val message = t.message ?: t.javaClass.simpleName
                mainHandler.post { finish(message) }
            }
        })
    }

    fun send(message: Wire.ClientMessage) {
        if (closed) return
        val ok = webSocket?.send(message.toJsonString()) ?: false
        if (!ok) {
            mainHandler.post { finish("send failed") }
        }
    }

    fun close() {
        if (closed) return
        closed = true
        onEvent = null
        webSocket?.close(1000, null)
        webSocket = null
        httpClient.dispatcher.executorService.shutdown()
        httpClient.connectionPool.evictAll()
    }

    private fun finish(reason: String) {
        if (closed) return
        val handler = onEvent
        close()
        handler?.invoke(Event.Closed(reason))
    }

    private class PinnedCertTrustManager(
        private val expectedHex: String,
    ) : X509TrustManager {
        override fun checkClientTrusted(chain: Array<out X509Certificate>?, authType: String?) = Unit

        override fun checkServerTrusted(chain: Array<out X509Certificate>?, authType: String?) {
            val leaf = chain?.firstOrNull()
                ?: throw CertificateException("empty server certificate chain")
            val digest = sha256Hex(leaf.encoded)
            if (digest != expectedHex) {
                Log.e(TAG, "certificate fingerprint mismatch")
                throw CertificateException("certificate fingerprint mismatch")
            }
        }

        override fun getAcceptedIssuers(): Array<X509Certificate> = emptyArray()
    }

    companion object {
        private const val TAG = "WaystoneSignal"

        fun sha256Hex(bytes: ByteArray): String =
            MessageDigest.getInstance("SHA-256")
                .digest(bytes)
                .joinToString("") { "%02x".format(it.toInt() and 0xff) }
    }
}
