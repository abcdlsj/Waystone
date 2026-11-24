import Foundation

/// The `waystone://pair?ep=…&turn=…&fp=…&tk=…` link rendered by the panel.
///
/// Mirrors `waystone_proto::pairing::PairingUri`; it is the only thing a user
/// ever hands the app, through the camera, a tapped link or a paste.
struct PairingLink: Equatable {
    /// Server authority, e.g. `home.example.com:8443`.
    var endpoint: String
    /// STUN/TURN authority, e.g. `home.example.com:3478`.
    var turn: String
    /// Pinned certificate fingerprint, lowercase hex.
    var fingerprint: String
    /// One-time enrollment token.
    var token: String

    init?(_ text: String) {
        let trimmed = text.trimmingCharacters(in: .whitespacesAndNewlines)
        guard let components = URLComponents(string: trimmed),
            components.scheme == "waystone",
            components.host == "pair"
        else { return nil }
        var values: [String: String] = [:]
        for item in components.queryItems ?? [] {
            if let value = item.value, !value.isEmpty { values[item.name] = value }
        }
        guard let endpoint = values["ep"],
            let turn = values["turn"],
            let fingerprint = values["fp"]?.lowercased(),
            fingerprint.count == 64,
            fingerprint.allSatisfy(\.isHexDigit),
            let token = values["tk"],
            token.count >= 16
        else { return nil }
        self.endpoint = endpoint
        self.turn = turn
        self.fingerprint = fingerprint
        self.token = token
    }
}
