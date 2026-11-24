import Foundation

/// A Waystone server this phone is enrolled with.
///
/// Everything needed to reconnect came from the pairing link. The pinned
/// certificate fingerprint doubles as the identity: it is stable for the life
/// of a server, and a server with another certificate is another server as
/// far as trust is concerned.
struct Instance: Codable, Identifiable, Equatable {
    var id: String { fingerprint }
    /// Server authority, e.g. `home.example.com:8443`.
    var endpoint: String
    /// STUN/TURN authority, e.g. `home.example.com:3478`.
    var turn: String
    /// Pinned certificate fingerprint, lowercase hex.
    var fingerprint: String
    /// Shown as the section title on the home screen.
    var label: String

    init(link: PairingLink) {
        self.endpoint = link.endpoint
        self.turn = link.turn
        self.fingerprint = link.fingerprint
        self.label = Self.defaultLabel(for: link.endpoint)
    }

    /// The host part of the endpoint, which is what people recognize.
    static func defaultLabel(for endpoint: String) -> String {
        var host = endpoint
        if host.hasPrefix("["), let close = host.firstIndex(of: "]") {
            return String(host[host.index(after: host.startIndex)..<close])
        }
        if let colon = host.lastIndex(of: ":") { host = String(host[..<colon]) }
        return host
    }
}
