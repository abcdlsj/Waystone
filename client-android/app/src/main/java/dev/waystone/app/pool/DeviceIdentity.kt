package dev.waystone.app.pool

import dev.waystone.app.protocol.Wire
import org.bouncycastle.crypto.params.Ed25519PrivateKeyParameters
import org.bouncycastle.crypto.signers.Ed25519Signer
import java.io.File
import java.security.MessageDigest
import java.security.SecureRandom
import java.util.Base64

/**
 * Ed25519 device identity persisted in app-private storage.
 *
 * The device id is derived the same way as in `waystone-proto`:
 * `hex(sha256(public_key))[0..16]`.
 */
class DeviceIdentity private constructor(
    private val privateKey: Ed25519PrivateKeyParameters,
) {
    interface Storage {
        fun load(key: String): ByteArray?
        fun save(data: ByteArray, key: String)
    }

    val publicKeyBytes: ByteArray = privateKey.generatePublicKey().encoded
    val publicKeyBase64: String = Base64.getEncoder().encodeToString(publicKeyBytes)
    val id: String = deviceId(publicKeyBytes)

    val wireIdentity: Wire.Identity
        get() = Wire.Identity(id = id, publicKey = publicKeyBase64)

    fun sign(payload: ByteArray): String {
        val signer = Ed25519Signer()
        signer.init(true, privateKey)
        signer.update(payload, 0, payload.size)
        return Base64.getEncoder().encodeToString(signer.generateSignature())
    }

    fun info(name: String, platform: Wire.Platform = Wire.Platform.ANDROID): Wire.DeviceInfo =
        Wire.DeviceInfo(
            id = id,
            name = name,
            platform = platform,
            role = Wire.Role.CONTROLLER,
            publicKey = publicKeyBase64,
        )

    companion object {
        fun loadOrCreate(key: String = "primary", storage: Storage): DeviceIdentity {
            val existing = storage.load(key)
            val seed = if (existing != null && existing.size == 32) {
                existing
            } else {
                ByteArray(32).also { SecureRandom().nextBytes(it) }.also { storage.save(it, key) }
            }
            return DeviceIdentity(Ed25519PrivateKeyParameters(seed, 0))
        }

        /** Mirrors `waystone_proto::device::DeviceId::from_public_key`. */
        fun deviceId(publicKey: ByteArray): String {
            val digest = MessageDigest.getInstance("SHA-256").digest(publicKey)
            return digest.take(8).joinToString("") { "%02x".format(it.toInt() and 0xff) }
        }
    }
}

/** App-private file storage for the raw 32-byte Ed25519 seed. */
class FileIdentityStorage(private val dir: File) : DeviceIdentity.Storage {
    override fun load(key: String): ByteArray? {
        val file = File(dir, "identity_$key.key")
        if (!file.isFile) return null
        return runCatching { file.readBytes() }.getOrNull()
    }

    override fun save(data: ByteArray, key: String) {
        dir.mkdirs()
        val target = File(dir, "identity_$key.key")
        val tmp = File(dir, "identity_$key.key.tmp")
        tmp.writeBytes(data)
        tmp.setReadable(false, false)
        tmp.setReadable(true, true)
        tmp.setWritable(false, false)
        tmp.setWritable(true, true)
        if (!tmp.renameTo(target)) {
            target.writeBytes(data)
            tmp.delete()
        }
    }
}
