import Foundation

/// One enrolled instance plus its long lived connection details.
struct Instance: Codable, Identifiable, Equatable {
    /// Server authority, e.g. `home.example.com:8443`.
    let endpoint: String
    /// Relay authority, e.g. `home.example.com:3478`.
    let turn: String
    /// Pinned certificate fingerprint, lowercase hex.
    let fingerprint: String
    /// Server instance id, used to tell two servers apart.
    var serverId: String
    /// Human readable label shown on the instance card.
    var label: String
    /// Last time this instance answered, for the "offline for 2h" hint.
    var lastSeen: Date?

    var id: String { serverId.isEmpty ? endpoint : serverId }

    /// Parses a `waystone://pair?...` URI.
    static func from(pairingURI text: String) -> Instance? {
        let trimmed = text.trimmingCharacters(in: .whitespacesAndNewlines)
        guard let queryRange = trimmed.range(of: "waystone://pair?") else { return nil }
        let query = String(trimmed[queryRange.upperBound...])
        var values: [String: String] = [:]
        for pair in query.split(separator: "&") {
            let parts = pair.split(separator: "=", maxSplits: 1, omittingEmptySubsequences: false)
            guard parts.count == 2 else { continue }
            let value = String(parts[1]).replacingOccurrences(of: "%3A", with: ":")
                .replacingOccurrences(of: "%5B", with: "[")
                .replacingOccurrences(of: "%5D", with: "]")
            values[String(parts[0])] = value
        }
        guard let endpoint = values["ep"],
            let turn = values["turn"],
            let fingerprint = values["fp"],
            let token = values["tk"],
            token.count >= 16
        else { return nil }
        return Instance(
            endpoint: endpoint,
            turn: turn,
            fingerprint: fingerprint.lowercased(),
            serverId: "",
            label: endpoint,
            lastSeen: nil
        )
    }

    /// Pairing token, only present between scanning and pairing.
    var pendingToken: String?

    init(
        endpoint: String,
        turn: String,
        fingerprint: String,
        serverId: String,
        label: String,
        lastSeen: Date?,
        pendingToken: String? = nil
    ) {
        self.endpoint = endpoint
        self.turn = turn
        self.fingerprint = fingerprint
        self.serverId = serverId
        self.label = label
        self.lastSeen = lastSeen
        self.pendingToken = pendingToken
    }

    /// Extracts the token from a pairing URI, if any.
    static func token(from uri: String) -> String? {
        guard let range = uri.range(of: "tk=") else { return nil }
        let rest = uri[range.upperBound...]
        return String(rest.split(separator: "&").first ?? "")
    }
}

/// A host visible to this controller.
struct Host: Identifiable, Equatable {
    var id: String
    var name: String
    var online: Bool
    var platform: String
    var rttMs: UInt32?

    init(peer: Wire.PeerState) {
        self.id = peer.device.id
        self.name = peer.device.name
        self.online = peer.online
        self.platform = peer.device.platform
        self.rttMs = peer.rttMs
    }
}

/// Connection quality shown in the floating status capsule.
struct PathStatus: Equatable {
    enum Path: Equatable {
        case direct
        case relayed
        case unknown
    }

    var path: Path = .unknown
    var rttMs: UInt32?
    var estimatedBitrate: UInt32?

    var label: String {
        switch path {
        case .direct:
            return rttMs.map { "Direct · \($0) ms" } ?? "Direct"
        case .relayed:
            return rttMs.map { "Relay · \($0) ms" } ?? "Relay"
        case .unknown:
            return "Connecting"
        }
    }

    var isHealthy: Bool {
        path != .unknown
    }
}
