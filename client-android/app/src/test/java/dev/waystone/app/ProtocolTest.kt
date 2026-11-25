package dev.waystone.app

import dev.waystone.app.media.NalSyntax
import dev.waystone.app.media.VideoDecoder
import dev.waystone.app.pool.DeviceIdentity
import dev.waystone.app.protocol.InputEvent
import dev.waystone.app.protocol.KeyCode
import dev.waystone.app.protocol.Modifiers
import dev.waystone.app.protocol.PairingLink
import dev.waystone.app.protocol.Wire
import dev.waystone.app.stream.Transport
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class ProtocolTest {
    @Test
    fun parsesValidPairingLinkAndRejectsBadOnes() {
        val fp = "A".repeat(64)
        val tk = "1234567890abcdef"
        val link = PairingLink.parse("waystone://pair?ep=home.example.com:8443&turn=home.example.com:3478&fp=$fp&tk=$tk")
        assertNotNull(link)
        assertEquals("home.example.com:8443", link!!.endpoint)
        assertEquals("home.example.com:3478", link.turn)
        assertEquals("a".repeat(64), link.fingerprint)
        assertEquals(tk, link.token)

        assertNull(PairingLink.parse("https://example.com"))
        assertNull(PairingLink.parse("waystone://pair?ep=a:1&turn=a:2&fp=short&tk=$tk"))
        assertNull(PairingLink.parse("waystone://pair?ep=a:1&turn=a:2&fp=$fp&tk=short"))
    }

    @Test
    fun encodesInputFramesByteForByte() {
        assertArrayEquals(
            byteArrayOf(0x01, 0x00, 0x00, 0xff.toByte(), 0xff.toByte()),
            InputEvent.MouseMove(0.0, 1.0).encoded(),
        )
        assertFalse(InputEvent.MouseMove(0.5, 0.5).isReliable)

        assertArrayEquals(
            byteArrayOf(0x02, 0x01, 0x01, 0x80.toByte(), 0x00, 0x00, 0x00),
            InputEvent.MouseButtonEvent(InputEvent.MouseButton.RIGHT, down = true, x = 0.5, y = 0.0).encoded(),
        )
        assertTrue(InputEvent.MouseButtonEvent(InputEvent.MouseButton.LEFT, down = true, x = 0.0, y = 0.0).isReliable)

        assertArrayEquals(
            byteArrayOf(0x04, 0x00, 0x00, 0x00, 0x2c, 0x01, Modifiers.CONTROL.toByte()),
            InputEvent.Key(KeyCode.SPACE, down = true, modifiers = Modifiers.CONTROL).encoded(),
        )

        assertArrayEquals(byteArrayOf(0x06), InputEvent.ReleaseAll.encoded())
    }

    @Test
    fun decodesServerMessages() {
        val challenge = Wire.ServerMessage.decode("""{"type":"challenge","nonce":"n1","server_id":"s1"}""")
        assertEquals(Wire.ServerMessage.Challenge("n1", "s1"), challenge)

        val accepted = Wire.ServerMessage.decode(
            """{"type":"session_accepted","session_id":"sid","video_codec":"h264","peer":{"id":"h1","name":"PC","platform":"windows","role":"host","public_key":"pk"}}"""
        )
        assertTrue(accepted is Wire.ServerMessage.SessionAccepted)
        assertEquals(Wire.VideoCodec.H264, (accepted as Wire.ServerMessage.SessionAccepted).videoCodec)
    }

    @Test
    fun derivesDeterministicEd25519Identity() {
        val memory = mutableMapOf<String, ByteArray>()
        val storage = object : DeviceIdentity.Storage {
            override fun load(key: String): ByteArray? = memory[key]
            override fun save(data: ByteArray, key: String) {
                memory[key] = data.copyOf()
            }
        }
        val first = DeviceIdentity.loadOrCreate("test", storage)
        val second = DeviceIdentity.loadOrCreate("test", storage)
        assertEquals(first.id, second.id)
        assertEquals(16, first.id.length)
        assertEquals(first.publicKeyBase64, second.publicKeyBase64)
        assertEquals(first.sign(Wire.signingPayload("s1", "n1")), second.sign(Wire.signingPayload("s1", "n1")))
    }

    @Test
    fun splitsAnnexBAndMediaHeader() {
        val annexB = byteArrayOf(0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 4)
        val units = VideoDecoder.splitAnnexB(annexB)
        assertEquals(3, units.size)
        assertTrue(NalSyntax.H264.isParameterSet(NalSyntax.H264.nalType(units[0][0])))
        assertTrue(NalSyntax.H264.isParameterSet(NalSyntax.H264.nalType(units[1][0])))
        assertTrue(NalSyntax.H264.isKeyframe(NalSyntax.H264.nalType(units[2][0])))

        val frame = ByteArray(20)
        frame[0] = 1
        frame[1] = 1
        frame[9] = 90
        frame[16] = 0x42
        val parsed = Transport.splitMedia(frame)
        assertNotNull(parsed)
        assertTrue(parsed!!.first.keyframe)
        assertEquals(90L, parsed.first.rtpTime)
        assertEquals(4, parsed.second.size)
        assertEquals(0x42.toByte(), parsed.second[0])
    }
}
