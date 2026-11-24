import CryptoKit
import Foundation

/// Ed25519 device identity, persisted in the keychain.
///
/// The private key never leaves the Secure Enclave backed keychain entry, and the
/// public key is the only thing the server ever sees. The device id is derived
/// the same way as in `waystone-proto`: `hex(sha256(public_key))[..16]`.
struct DeviceIdentity {
    let id: String
    let publicKeyBase64: String
    private let storage: Storage

    /// Minimal abstraction so the identity can be created in tests without a
    /// real keychain.
    protocol Storage {
        func load(key: String) -> Data?
        func save(_ data: Data, key: String) throws
    }

    enum KeychainError: Error {
        case unexpectedStatus(OSStatus)
        case missingKey
    }

    init(loadOrCreateFor key: String, storage: Storage = KeychainStorage()) throws {
        self.storage = storage
        let privateKey: Curve25519.Signing.PrivateKey
        if let existing = storage.load(key: key) {
            privateKey = try Curve25519.Signing.PrivateKey(rawRepresentation: existing)
        } else {
            privateKey = Curve25519.Signing.PrivateKey()
            try storage.save(privateKey.rawRepresentation, key: key)
        }
        let publicBytes = privateKey.publicKey.rawRepresentation
        self.publicKeyBase64 = publicBytes.base64EncodedString()
        self.id = Self.deviceId(from: publicBytes)
        self.privateKey = privateKey
    }

    private let privateKey: Curve25519.Signing.PrivateKey

    /// Signs a payload and returns the base64 signature the server expects.
    func sign(_ payload: Data) -> String {
        let signature = try? privateKey.signature(for: payload)
        return signature?.base64EncodedString() ?? ""
    }

    /// Builds the `DeviceInfo` sent during pairing and authentication.
    func info(name: String, platform: String) -> Wire.DeviceInfo {
        Wire.DeviceInfo(
            id: id,
            name: name,
            platform: platform,
            role: "controller",
            publicKey: publicKeyBase64
        )
    }

    /// Mirrors `waystone_proto::device::DeviceId::from_public_key`.
    static func deviceId(from publicKey: Data) -> String {
        let digest = SHA256.hash(data: publicKey)
        return digest.prefix(8).map { String(format: "%02x", $0) }.joined()
    }
}

/// Keychain backed `Storage` production implementation.
final class KeychainStorage: DeviceIdentity.Storage {
    private let service: String

    init(service: String = "dev.waystone.identity") {
        self.service = service
    }

    func load(key: String) -> Data? {
        var query = baseQuery(key: key)
        query[kSecReturnData as String] = true
        query[kSecMatchLimit as String] = kSecMatchLimitOne

        var item: CFTypeRef?
        let status = SecItemCopyMatching(query as CFDictionary, &item)
        guard status == errSecSuccess else { return nil }
        return item as? Data
    }

    func save(_ data: Data, key: String) throws {
        var query = baseQuery(key: key)
        SecItemDelete(query as CFDictionary)
        query[kSecValueData as String] = data
        // The key is only usable while the device is unlocked, and is never
        // synced to iCloud: it identifies this phone, not the user.
        query[kSecAttrAccessible as String] = kSecAttrAccessibleWhenUnlockedThisDeviceOnly
        let status = SecItemAdd(query as CFDictionary, nil)
        guard status == errSecSuccess else {
            throw DeviceIdentity.KeychainError.unexpectedStatus(status)
        }
    }

    private func baseQuery(key: String) -> [String: Any] {
        [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: key,
        ]
    }
}
